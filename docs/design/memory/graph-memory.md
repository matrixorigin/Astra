# Graph Memory Backend

> **Status**: Design Complete — implementation planned (8 weeks)
> **Last Updated**: 2026-03-08
> **Scope**: Graph-based memory with spreading activation retrieval and three-phase reflection
> **Config**: `memory_backend = "graph"`
> **Overview**: See [memory-overview.md](memory-overview.md) for shared concepts (cognitive architecture, context engineering, protocols)
> **Alternative**: See [tabular-memory.md](tabular-memory.md) for the production tabular backend
> **Coexistence**: See [backend-coexistence.md](backend-coexistence.md) for how tabular/graph coexist

---

## Executive Summary

The current memory system excels at storage, retrieval, and governance but lacks the ability to **learn from experience**. Memories are flat rows retrieved by vector similarity — fundamentally the same as 2023-era RAG. Recent SOTA systems (Synapse, EverMemOS, Hindsight) demonstrate that **graph-structured memory with activation dynamics** dramatically outperforms flat retrieval:

- **+23% multi-hop reasoning** accuracy (Synapse vs A-Mem on LoCoMo)
- **95% token reduction** vs full-context methods
- **96.6% adversarial robustness** via lateral inhibition
- **91.4% overall accuracy** on LongMemEval (Hindsight, vs 39% baseline)

This document proposes three interconnected upgrades:

1. **Memory Graph** — replace flat memory rows with a typed graph (episodic → semantic → scene nodes connected by typed edges)
2. **Spreading Activation Retrieval** — replace pure vector similarity with activation propagation that discovers structurally relevant but semantically distant memories
3. **Three-Phase Reflection** — Perceive (per-turn) → Consolidate (periodic) → Reflect (daily) pipeline that enables the agent to generalize from experience

---

## 1. Problem Statement

### What We Have

```
User query → Vector search → Top-K memories → Stuff into prompt → LLM
```

This works for direct recall ("what's the user's name?") but fails at:

| Failure Mode | Example | Root Cause |
|---|---|---|
| **Multi-hop reasoning** | "Why does CI keep failing?" requires linking: CI config → Docker version → base image change across 3 sessions | Vector similarity can't bridge semantically distant but causally linked facts |
| **Pattern recognition** | User has asked about the same error 5 times across sessions — agent doesn't notice the pattern | No cross-session aggregation beyond session summaries |
| **Temporal reasoning** | "When did we switch from pytest to unittest?" requires ordering events | Flat retrieval has no temporal structure |
| **Contradiction evolution** | User said "I prefer Go" then later "Actually Python is better for this" | Supersede chain exists but no belief evolution with confidence tracking |
| **Contextual tunneling** | Agent retrieves semantically similar but logically irrelevant memories | No mechanism to suppress distractors |

### What SOTA Systems Prove

| System | Key Innovation | Benchmark Result |
|---|---|---|
| **Synapse** (2026.01) | Spreading Activation on episodic-semantic graph | LoCoMo SOTA: 40.5 F1 (vs 33.3 A-Mem), Multi-hop +23% |
| **EverMemOS** (2026.01) | MemCell → MemScene semantic consolidation | LoCoMo 92.3%, LongMemEval-S 82% |
| **Hindsight** (2025.12) | 4-network separation + Retain/Recall/Reflect | LongMemEval 91.4% (vs 39% full-context baseline) |
| **A-Mem** (2025.02) | Zettelkasten self-organizing links | LoCoMo SOTA at time of publication |

**Common thread**: all top systems use **graph structure** + **typed relationships** + **consolidation/reflection**. None use flat vector retrieval alone.

### What We Need

```
User query → Dual Trigger (BM25 + Vector)
           → Spreading Activation on Memory Graph
           → Lateral Inhibition (suppress distractors)
           → Top-K activated nodes → Context
           
Background: Perceive → Consolidate → Reflect → Evolve
```

---

## 2. Architecture Overview

### 2.1 Three-Layer Memory Graph

The core data structure is a **typed directed graph** `G = (V, E)` where nodes represent memories at different abstraction levels and edges encode relationships.

```
┌─────────────────────────────────────────────────────────────────────┐
│                        MEMORY GRAPH                                 │
│                                                                     │
│  Layer 3: Scene Nodes (反思产出)                                     │
│  ┌─────────────────────────────────────────────────────────┐        │
│  │ "User repeatedly hits CI config issues after Docker     │        │
│  │  upgrades — root cause is pinned base image versions"   │        │
│  │  confidence: 0.75 | trust: T4 | importance: 0.82       │        │
│  └──────────────────────┬──────────────────────────────────┘        │
│                         │ consolidation edges                       │
│  Layer 2: Semantic Nodes (提取的知识)                                │
│  ┌──────────┐  association  ┌──────────┐  association  ┌─────────┐  │
│  │"user     │◄────────────▶│"project  │◄────────────▶│"CI uses │  │
│  │ prefers  │              │ uses     │              │ Docker  │  │
│  │ Go"      │              │ DI       │              │ 24.0"   │  │
│  │ c:0.6    │              │ pattern" │              │ c:0.9   │  │
│  └────┬─────┘              └────┬─────┘              └────┬────┘  │
│       │ abstraction             │ abstraction             │        │
│  Layer 1: Episodic Nodes (事件引用)                        │        │
│  ┌──────┐ temporal ┌──────┐ temporal ┌──────┐ temporal ┌──────┐   │
│  │ e:01 │────────▶│ e:02 │────────▶│ e:03 │────────▶│ e:04 │   │
│  │ sess1│         │ sess1│         │ sess2│  causal  │ sess2│   │
│  └──────┘         └──────┘         └──────┘────────▶└──────┘   │
│                                                                     │
│  Edge Types:                                                        │
│    temporal      e→e  Sequential events (weight: 1.0)               │
│    abstraction   e↔s  Event grounds a concept (weight: 0.8)         │
│    association   s↔s  Concepts co-occur or relate (weight: cosine)  │
│    causal        *→*  Cause-effect (weight: 1.0, boosted in SA)     │
│    consolidation s→sc Scene synthesized from semantics (weight: 1.0)│
└─────────────────────────────────────────────────────────────────────┘
```

**Node types:**

| Type | Source | Mutability | Lifetime | Storage |
|------|--------|-----------|----------|---------|
| `episodic` | conversation_events (reference, not copy) | Immutable | Follows event retention | `memory_graph_nodes.event_id → agent_events` |
| `semantic` | Observer extraction (per-turn) | Evolving (supersede chain) | Confidence decay | `memory_graph_nodes.memory_id → memories` |
| `scene` | Reflector synthesis (periodic) | Evolving (opinion reinforcement) | Confidence decay | `memory_graph_nodes` (self-contained) |

**Key design decision**: Episodic nodes are **references** to `agent_events`, not copies. This avoids data duplication and preserves the event sourcing audit trail. Semantic nodes reference the existing `memories` table. Only scene nodes are fully self-contained in the graph.

### 2.2 System Integration

```
Per-turn (inline, <50ms):                    Periodic (async):
┌──────────────┐                             ┌──────────────────┐
│   Observer    │                             │ GovernanceScheduler│
│   (existing)  │                             │   .run_daily()   │
│   extract +   │                             └────────┬─────────┘
│   persist     │                                      │
└──────┬───────┘                                      ▼
       │                                      ┌──────────────────┐
       ▼                                      │    Reflector     │
┌──────────────┐                              │  .consolidate()  │
│ GraphBuilder │                              │  .reflect()      │
│  .ingest()   │                              └────────┬─────────┘
│  - create    │                                       │
│    episodic  │                                       ▼
│    node      │                              ┌──────────────────┐
│  - create    │                              │  Memory Graph    │
│    semantic  │─────────────────────────────▶│  (shared state)  │
│    nodes     │                              └────────┬─────────┘
│  - build     │                                       │
│    edges     │                                       ▼
└──────────────┘                              ┌──────────────────┐
                                              │ActivationRetriever│
       Query path:                            │  .retrieve()     │
       User query ──▶ ActivationRetriever ───▶│  - dual trigger  │
                                              │  - propagate     │
                                              │  - inhibit       │
                                              │  - rank          │
                                              └──────────────────┘
```

**Integration points with existing system:**

| Component | Change | Rationale |
|---|---|---|
| `TypedObserver` | After persist, call `GraphBuilder.ingest()` | Build graph incrementally as memories are created |
| `MemoryRetriever` | Add `activation_retrieve()` path alongside existing hybrid | Gradual migration; existing path remains as fallback |
| `GovernanceScheduler.run_daily()` | Add `Reflector.consolidate()` and `Reflector.reflect()` | Reflection runs in existing governance cycle |
| `InputFaceLearner` | Consume reflection signals (scene creation/evolution) | Close the learning loop |
| `SessionSummarizer` | Output feeds into scene node creation | Summaries become consolidation input |

---

## 3. Spreading Activation Retrieval

### 3.1 Why Not Just Vector Search

Vector search finds memories that **look similar** to the query. Spreading activation finds memories that are **structurally relevant** — connected through causal chains, shared entities, temporal proximity, or transitive relationships.

**Concrete example:**

```
Query: "Why does CI keep failing?"

Vector search returns:
  1. "CI pipeline runs on Docker 24.0" (high cosine similarity to "CI")
  2. "User prefers Go over Python" (irrelevant but mentions "CI" in context)
  3. "Last CI run failed with exit code 137" (relevant, recent)

Spreading activation returns:
  1. "Last CI run failed with exit code 137" (direct semantic match)
  2. "Docker base image was updated 3 days ago" (causal link from CI failure)
  3. "Same OOM error occurred in session 5 and session 12" (scene node — pattern)
  4. "User pinned Docker version after similar issue last month" (procedural — solution)
  
  NOT returned (suppressed by lateral inhibition):
  - "User prefers Go over Python" (activated but inhibited by stronger CI-related nodes)
```

### 3.2 Algorithm

The retrieval algorithm follows Synapse (Jiang et al., 2026) adapted for our graph structure.

**Phase 1: Dual Trigger Initialization**

```
Input: query Q, user_id, token_budget K

1. Lexical trigger (BM25):
   R_lex = MatrixOne fulltext search on memory_graph_nodes.content
   → captures exact entity matches ("Docker", "pytest", proper nouns)

2. Semantic trigger (Vector):
   q_embed = embed(Q)
   R_sem = MatrixOne L2_DISTANCE search on memory_graph_nodes.embedding
   → captures conceptual similarity

3. Anchor set T = top-k from R_lex ∪ R_sem (deduplicated)

4. Initialize activation vector:
   a_i(0) = α · cosine_sim(node_i.embedding, q_embed)  if node_i ∈ T
   a_i(0) = 0                                            otherwise
```

**Phase 2: Activation Propagation (3 iterations)**

```
For t = 0, 1, 2:

  For each node i with neighbors N(i):

    # Raw activation potential
    u_i(t+1) = (1 - δ) · a_i(t)                    # retention
             + Σ_{j ∈ N(i)} S · w_ji · a_j(t)      # spreading
                              / fan(j)               # fan effect

    where:
      δ = 0.5          (decay rate)
      S = 0.8          (spreading factor)
      fan(j) = out_degree(j)  (attention dilution)
      w_ji = edge weight × edge_type_multiplier:
        temporal:      e^(-ρ · |τ_i - τ_j|)   (ρ = 0.01, time decay)
        abstraction:   0.8
        association:   cosine_sim(embed_i, embed_j)
        causal:        1.5  (boosted — causal links are high-value)
        consolidation: 1.0

  # Lateral inhibition (suppress distractors)
  Top-M = top 7 nodes by u_i(t+1)
  For each node i:
    û_i(t+1) = max(0, u_i(t+1)
               - β · Σ_{k ∈ Top-M} (u_k(t+1) - u_i(t+1))
                     · I[u_k(t+1) > u_i(t+1)])
    where β = 0.15

  # Sigmoid activation (non-linear firing)
  a_i(t+1) = sigmoid(γ · (û_i(t+1) - θ))
    where γ = 5.0, θ = 0.1
```

**Phase 3: Scoring and Selection**

```
Temporal decay function (mirrors existing trust tier half-lives):
  decay(node) = 0.5 ^ (age_days / half_life(node.trust_tier))

  where half_life:
    T1_VERIFIED:   365 days
    T2_CURATED:    180 days
    T3_INFERRED:    60 days
    T4_UNVERIFIED:  30 days

  This reuses the EXACT same decay model as the existing memory system
  (see memory-overview.md §1, TRUST_TIER_HALF_LIVES).
  A T4 node from 30 days ago has effective weight 0.5.
  A T1 node from 30 days ago has effective weight 0.94.

Effective confidence (query-time, not stored):
  effective_confidence(node) = node.confidence × decay(node)

Final score for each node:
  S(node_i) = λ1 · cosine_sim(embed_i, q_embed)       # semantic signal
            + λ2 · a_i(T)                               # activation signal
            + λ3 · effective_confidence(node_i)          # trust × recency
            + λ4 · node_i.importance                     # structural importance

  where λ = (0.35, 0.35, 0.20, 0.10)

  Conflict penalty (applied after scoring):
    if node.conflict_resolution == 'superseded': S *= 0.5
    if node.conflict_resolution == 'pending':    S *= 0.7

Select top nodes within token budget K.
Return nodes sorted by S(node_i), with graph context (connected edges).
```

**Why temporal decay matters**: Without it, a 6-month-old T4 node ("user prefers tabs over spaces") scores the same as a yesterday's T1 node ("project switched to Go"). The decay function naturally deprioritizes stale, low-trust memories while preserving verified knowledge — exactly matching human memory behavior where unverified information fades faster than confirmed facts.

### 3.3 Complexity and Performance

| Operation | Tier 1 (<10K) | Tier 2 (10K-50K) | Tier 3 (>50K) |
|---|---|---|---|
| Graph load | Full: <20ms | Skeleton: <50ms | Skip (no full load) |
| Anchor selection | In-memory | DB-side (BM25+vector): <20ms | DB-side: <20ms |
| Hop expansion | N/A (all in memory) | N/A (skeleton in memory) | 3 queries × <10ms = <30ms |
| Activation (3 rounds) | <30ms on full graph | <30ms on skeleton | <10ms on ~2,500 node subgraph |
| Embedding fetch | In cache | Top-K only: <5ms | Top-K only: <5ms |
| **Total retrieval** | **<60ms** | **<80ms** | **<70ms** |

**Key insight**: Spreading activation complexity is `O(iterations × working_set × avg_degree)`. By controlling the working set size (Tier 2: skeleton without embeddings; Tier 3: local subgraph), latency stays bounded regardless of total graph size.

**Node archival** (all tiers): Nodes with activation consistently < ε=0.01 across W=10 retrieval windows → `is_active = 0`. Archived nodes still queryable via direct vector search (fallback path). This keeps the active graph from growing without bound.

### 3.4 Fallback Strategy

Spreading activation is the primary retrieval path, but the existing `MemoryRetriever.retrieve()` remains as fallback:

```python
def retrieve(self, query, user_id, ..., use_activation=True):
    if use_activation and self._graph_available(user_id):
        return self._activation_retrieve(query, user_id, ...)
    else:
        return self._legacy_hybrid_retrieve(query, user_id, ...)
```

This ensures zero regression during rollout — users without graph data get the existing behavior.

---

## 4. Three-Phase Memory Lifecycle

### 4.1 Phase 1: Perceive (Per-Turn, Inline)

**Trigger**: After every `TypedObserver.observe()` call.
**Latency budget**: <50ms additional.
**LLM calls**: 0 (graph building is structural, not generative).

```
Observer.observe() completes
    │
    │  Returns: list[Memory] (newly persisted semantic memories)
    │  Plus: source events from conversation_events
    │
    ▼
GraphBuilder.ingest(user_id, new_memories, source_events)
    │
    ├── 1. Create episodic nodes (if not exist)
    │      For each source event:
    │        - node_type = 'episodic'
    │        - event_id = event.event_id (reference, not copy)
    │        - embedding = event_embeddings (already computed async)
    │        - Build temporal edge to previous episodic node in session
    │
    ├── 2. Create/update semantic nodes
    │      For each new memory:
    │        - node_type = 'semantic'
    │        - memory_id = memory.memory_id (reference)
    │        - embedding = memory.embedding
    │        - Build abstraction edges: episodic ↔ semantic
    │
    ├── 3. Build association edges (semantic ↔ semantic)
    │      For each new semantic node:
    │        - Find top-5 existing semantic nodes by L2_DISTANCE
    │        - If cosine_sim > 0.7: create association edge
    │        - Weight = cosine_sim
    │
    └── 4. Detect causal edges (heuristic, no LLM)
           If event is tool_error and previous event is tool_call:
             - Create causal edge: tool_call → tool_error
           If user_query references previous llm_response:
             - Create causal edge: llm_response → user_query
```

### 4.2 Phase 2: Consolidate (Every N Turns or Time-Based)

**Trigger**: Every 10 turns within a session, or every 2 hours of active session.
**Latency budget**: <500ms (async, non-blocking).
**LLM calls**: 0 (structural operations only).

Consolidation maintains graph health — merging duplicates, strengthening frequent associations, and pruning noise. Inspired by EverMemOS MemCell→MemScene first stage.

```
Reflector.consolidate(user_id)
    │
    ├── 1. Merge near-duplicate semantic nodes
    │      Find pairs where L2_DISTANCE < 0.15 (very similar)
    │      AND same memory_type
    │      → Keep higher-confidence node
    │      → Transfer edges from merged node
    │      → Deactivate merged node (supersede chain)
    │
    ├── 2. Strengthen frequent associations
    │      For association edges traversed in recent retrievals:
    │        weight = min(1.0, weight + 0.05)
    │      For association edges NOT traversed in 30 days:
    │        weight = max(0.0, weight - 0.1)
    │      If weight <= 0: remove edge
    │
    ├── 3. Detect contradictions across sessions
    │      For each semantic node pair with:
    │        - Same entity/topic (high association weight)
    │        - Different content (low content similarity)
    │        - Different sessions
    │      → Flag for reflection (Phase 3 input)
    │
    └── 4. Update graph statistics
           Per-node: access_count, last_accessed, cross_session_count
           Per-user: total_nodes, total_edges, avg_degree
```

### 4.3 Phase 3: Reflect (Daily + Event-Triggered)

> Reflection uses the shared `ReflectionEngine` from `core/memory/reflection/`. The graph backend provides a `GraphCandidateProvider` that feeds it high-activation subgraphs. The engine's importance scoring, LLM synthesis, opinion evolution, and trust tier promotion are backend-agnostic. See [backend-coexistence.md §2](backend-coexistence.md) for the module layout.

**Trigger**: 
- **Scheduled**: `GovernanceScheduler.run_daily()` — process past 24h
- **Event-driven**: When `importance_score >= 0.7` (see §4.4)

**Latency budget**: <5s per user (async background task).
**LLM calls**: 1 per cluster that passes importance threshold.

This is the core innovation — the agent generalizes from experience.

```
Reflector.reflect(user_id, days=1)
    │
    ├── 1. Collect high-activation subgraphs
    │      Run spreading activation with recent events as anchors
    │      Identify connected components with activation > threshold
    │      Each component = a candidate "theme"
    │
    ├── 2. Filter by importance (see §4.4)
    │      Score each component
    │      Skip components with importance < 0.5
    │      Skip components already reflected (source_node_ids overlap > 80%)
    │      Skip single-session components (Observer already handles these)
    │
    ├── 3. Synthesize insights (LLM call)
    │      For each qualifying component:
    │        Input:
    │          - Component node contents (truncated to ~2000 chars)
    │          - Existing related scene nodes (avoid repetition)
    │          - Existing related semantic memories
    │        Prompt: (see §4.6 Reflection Prompt)
    │        Output constraints:
    │          - type ∈ {semantic, procedural}
    │          - confidence ∈ [0.3, 0.7] (conservative initial)
    │          - content must be actionable, not just descriptive
    │
    ├── 4. Create scene nodes
    │      For each insight:
    │        - node_type = 'scene'
    │        - content = insight text
    │        - confidence = LLM-assigned (capped at 0.7 for new scenes)
    │        - trust_tier = T4_UNVERIFIED (must earn trust)
    │        - source_node_ids = all nodes in the component
    │        - importance = component importance score
    │        - Build consolidation edges: source semantic nodes → scene
    │
    └── 5. Record audit event
           Log 'memory_reflection' event to agent_events
           Content: {components_found, scenes_created, importance_scores}
           → Full audit trail via event sourcing
```

### 4.4 Importance Scoring

Multi-signal importance scoring determines what is worth reflecting on. No LLM calls — pure heuristics over graph signals.

```
def score_importance(component: list[GraphNode]) -> float:
    """Score a subgraph component for reflection worthiness."""
    signals = []

    # Signal 1: Structural centrality (0.0 - 1.0)
    # High activation energy = many paths converge here = structurally important
    avg_activation = mean(node.activation for node in component)
    signals.append(('centrality', 0.25, avg_activation))

    # Signal 2: Cross-session span (0.0 - 1.0)
    # Patterns spanning multiple sessions are more valuable than single-session
    session_ids = unique(node.session_id for node in component if node.session_id)
    cross_session = min(len(session_ids) / 3.0, 1.0)
    signals.append(('cross_session', 0.25, cross_session))

    # Signal 3: Contradiction/correction signal (0.0 - 1.0)
    # Contradictions and user corrections demand reflection
    has_contradiction = any(node.has_contradiction_flag for node in component)
    has_correction = any(
        node.feedback_signal in ('correction', 'frustration')
        for node in component
    )
    correction = 1.0 if has_contradiction else (0.7 if has_correction else 0.0)
    signals.append(('correction', 0.30, correction))

    # Signal 4: Recurrence frequency (0.0 - 1.0)
    # Topics that keep coming up are worth reflecting on
    recurrence = min(len(component) / 5.0, 1.0)
    signals.append(('recurrence', 0.20, recurrence))

    return sum(weight * value for _, weight, value in signals)
```

**Threshold calibration:**
- `>= 0.7`: Immediate reflection (event-triggered)
- `>= 0.5`: Queued for daily reflection
- `< 0.5`: Skip (not worth the LLM call)

### 4.5 Opinion Evolution

Scene nodes are not static. They evolve as new evidence arrives — inspired by Hindsight's opinion reinforcement.

```
On each new episodic event:
    │
    ├── Run lightweight activation (1 iteration, anchored on new event)
    │
    ├── Find activated scene nodes (activation > 0.3)
    │
    └── For each activated scene:
          │
          ├── Evidence alignment check (heuristic + optional LLM):
          │     Cosine similarity between new event and scene content
          │     If sim > 0.8: SUPPORTING evidence
          │     If sim < 0.3 AND same topic: CONTRADICTING evidence
          │     Otherwise: NEUTRAL
          │
          ├── Update confidence:
          │     SUPPORTING:    confidence += 0.05 (capped at 0.95)
          │     CONTRADICTING: confidence -= 0.10
          │     NEUTRAL:       no change
          │
          ├── Trust tier promotion:
          │     If confidence > 0.8 AND trust_tier == T4:
          │       promote to T3_INFERRED
          │     If confidence > 0.9 AND confirmed by user:
          │       promote to T2_CURATED
          │
          └── Quarantine:
                If confidence < 0.2:
                  is_active = 0 (quarantine)
                  Log 'scene_quarantined' event
```

**Why this matters**: A scene like "User prefers verbose error messages" starts at confidence 0.5 / T4. Each time the user's behavior confirms it, confidence rises. If the user starts preferring concise output, confidence drops and eventually the scene is quarantined. The agent's beliefs evolve with evidence — not just stored and forgotten.

### 4.6 Reflection Prompt

The reflection prompt is critical for scene quality. Here is the full template:

```
SYSTEM:
You are analyzing an agent's experiences across multiple sessions with the same user.
Your goal: extract 1-2 reusable insights that will help the agent serve this user better.

RULES:
- Each insight must be ACTIONABLE (a behavioral rule or factual pattern), not just descriptive
- Each insight must be grounded in the evidence — do not speculate beyond what's shown
- If the experiences don't reveal a clear pattern, return an empty array []
- Assign confidence conservatively: 0.3-0.5 for weak patterns, 0.5-0.7 for strong ones
- Type "procedural" = how to do something; "semantic" = what is true about the user/project

EXISTING KNOWLEDGE (do not repeat these):
{existing_scene_contents}

EXPERIENCES TO ANALYZE:
{component_node_contents}

OUTPUT FORMAT (JSON array, 0-2 items):
[
  {
    "type": "procedural" | "semantic",
    "content": "One clear sentence describing the insight",
    "confidence": 0.3-0.7,
    "evidence_summary": "Which experiences support this (one sentence)"
  }
]
```

**Prompt evolution**: This prompt is managed by the existing `PromptOptimizer` system. It has a `prompt_template_id = "reflection_synthesis"` and follows the standard prompt lifecycle (version, A/B test, feedback-driven optimization). See [prompt-lifecycle.md](prompt-lifecycle.md).

### 4.7 Trust Tier Promotion Path

Scene nodes start at T4 (unverified, 30-day half-life). The full promotion path:

```
T4_UNVERIFIED (birth)
  │  confidence starts at 0.3-0.7 (LLM-assigned)
  │  half-life: 30 days — fades fast if not reinforced
  │
  │  Opinion evolution: each supporting event → confidence += 0.05
  │  After ~6 supporting events: confidence > 0.8
  │
  ▼
T3_INFERRED (auto-promoted)
  │  Trigger: confidence > 0.8 sustained for 7+ days
  │  half-life: 60 days — fades slower
  │  This is the highest tier reachable without human input
  │
  │  Mechanism: consolidation checks daily:
  │    if node.trust_tier == 'T4'
  │       and node.confidence > 0.8
  │       and age_days(node) > 7:
  │      promote to T3
  │
  ▼
T2_CURATED (human-confirmed)
  │  Trigger: user explicitly confirms or edits the insight
  │  Via: MemoryWriteTool with trust_tier override
  │  half-life: 180 days
  │
  ▼
T1_VERIFIED (admin/system-verified)
  │  Trigger: admin verification or automated regression gate pass
  │  half-life: 365 days — near-permanent
  │
  Note: T1/T2 are rare for scene nodes. Most scenes live at T3-T4.
  This is correct — reflections are inferences, not verified facts.
```

**Demotion path** (equally important):
```
Any tier → confidence drops below 0.2 → quarantine (is_active = 0)
T3 → no supporting evidence for 60 days → demote to T4
T2 → contradicted by user → demote to T3
```

---

## 5. Data Model

### 5.1 Design Decisions: Adjacency List in Nodes

**Problem with separate edge table**: Graph traversal requires repeated JOINs between nodes and edges. For 3-hop spreading activation, that's 3 JOINs per iteration × 3 iterations = 9 JOINs in the hot path. This is unacceptable for a <60ms latency target.

**Solution**: Store adjacency lists directly in the node row. Each node carries its outgoing edges as a JSON array. This converts multi-hop traversal from `O(hops × JOIN)` to `O(1) batch fetch + Python-side traversal`.

```
Traditional graph DB approach (what we DON'T do):
  SELECT * FROM edges WHERE source_id = :id  -- per node, per hop
  → 3 hops × avg 5 neighbors = 15 queries minimum

Our approach (adjacency-in-node):
  SELECT * FROM nodes WHERE node_id IN (:batch_ids)  -- one query per hop
  → 3 hops = 3 queries total, each fetches full node + adjacency
```

**Trade-off**: Edge updates require updating the source node's `edges_out` JSON. This is acceptable because:
- Edges are created at ingest time (write-once for most edge types)
- Association edge weight updates happen in consolidation (async, batch)
- The hot path (retrieval) is read-heavy, write-rare

### 5.2 Schema

```sql
CREATE TABLE memory_graph_nodes (
    node_id         VARCHAR(32) PRIMARY KEY,
    user_id         VARCHAR(64) NOT NULL,
    node_type       ENUM('episodic', 'semantic', 'scene') NOT NULL,

    -- Content (denormalized for all types — avoids JOIN to source tables)
    content         TEXT NOT NULL,
    embedding       VECF32(1536),

    -- References to source-of-truth tables (NULL for scene nodes)
    event_id        VARCHAR(32),       -- episodic → agent_events.event_id
    memory_id       VARCHAR(32),       -- semantic → memories.memory_id
    session_id      VARCHAR(64),       -- which session created this node

    -- Confidence and trust (query-time decay via effective_confidence())
    confidence      FLOAT DEFAULT 0.75,
    trust_tier      VARCHAR(4) DEFAULT 'T3',

    -- Importance: computed at ingest time, refreshed in consolidation.
    -- NOT a static default — see §5.3 for computation.
    -- Used by: reflection filtering, activation scoring, archival decisions.
    importance      FLOAT NOT NULL DEFAULT 0.0,

    -- Adjacency list (THE KEY OPTIMIZATION)
    -- Format: [{"id": "node_id", "t": "causal", "w": 1.5}, ...]
    edges_out       JSON DEFAULT '[]',
    edges_in        JSON DEFAULT '[]',

    -- Scene-specific: source node IDs as comma-separated string.
    -- Integrity: orphan detection runs in consolidation (§5.4).
    source_nodes    TEXT,              -- "id1,id2,id3" (scene only)

    -- Conflict tracking: links to the node this one contradicts/supersedes.
    -- NULL = no known conflict. Set by consolidation conflict detection (§5.5).
    conflicts_with  VARCHAR(32),       -- node_id of contradicted node
    conflict_resolution ENUM('pending', 'kept', 'superseded', 'merged') DEFAULT NULL,

    -- Graph statistics (batch-updated by consolidation, not per-access)
    access_count    INT DEFAULT 0,
    cross_session_count INT DEFAULT 0,

    -- Lifecycle
    is_active       TINYINT DEFAULT 1,
    superseded_by   VARCHAR(32),
    created_at      DATETIME DEFAULT UTC_TIMESTAMP(),

    -- Indexes
    INDEX idx_user_active (user_id, is_active, node_type),
    INDEX idx_event (event_id),
    INDEX idx_memory (memory_id),
    INDEX idx_conflicts (user_id, conflicts_with),
    INDEX idx_embedding USING IVFFLAT (embedding) LISTS = 100,
    FULLTEXT INDEX idx_content (content)
);
```

**What changed vs tabular and why:**

| Change | Rationale |
|---|---|
| `edges_out` + `edges_in` JSON in node row, no separate edge table | Eliminates all JOINs in traversal hot path |
| `importance FLOAT NOT NULL` with ingest-time computation | Never 0.0 by default — computed at creation, refreshed in consolidation (§5.3) |
| `conflicts_with` + `conflict_resolution` columns | Explicit conflict tracking between contradictory nodes (§5.5) |
| No foreign keys | FK constraints add write overhead. Consistency enforced at application layer |
| No `updated_at` column | Node evolution uses supersede chain (new node, old deactivated) |
| `source_nodes` as TEXT not JSON | `LIKE '%node_id%'` works with standard indexes. Orphan detection in consolidation (§5.4) |

### 5.3 Importance Computation

**Problem**: `importance DEFAULT 0.0` is useless — every node starts at zero and stays there unless something explicitly updates it.

**Solution**: Importance is computed at two points:
1. **At ingest time** (per-node, lightweight) — every node gets a non-zero importance at birth
2. **At consolidation** (batch refresh) — importance is recalculated with graph-level signals

```python
def compute_node_importance(node_type: str, event: dict | None,
                            memory: Memory | None,
                            neighbor_count: int) -> float:
    """Compute importance at ingest time. No LLM, no graph traversal."""
    base = {
        "episodic": 0.3,   # events start moderate
        "semantic": 0.5,   # extracted knowledge starts higher
        "scene": 0.6,      # reflections start highest
    }[node_type]

    boost = 0.0

    # Event-type signal (episodic nodes)
    if event:
        if event.get("event_type") == "tool_error":
            boost += 0.2   # errors are important
        if event.get("event_type") == "user_query":
            content = event.get("content", "")
            # Correction/frustration patterns (reuse ImplicitFeedbackDetector)
            if _is_correction_or_frustration(content):
                boost += 0.25

    # Confidence signal (semantic nodes)
    if memory and memory.initial_confidence >= 0.85:
        boost += 0.1       # high-confidence extractions matter more

    # Connectivity signal
    if neighbor_count >= 3:
        boost += 0.1       # well-connected nodes are structurally important

    return min(base + boost, 1.0)
```

**Consolidation refresh** (runs daily, batch):
```python
def refresh_importance(graph: InMemoryGraph, node: GraphNode) -> float:
    """Recalculate importance with graph-level signals."""
    signals = [
        node.importance * 0.3,                              # prior importance (momentum)
        min(len(node.edges_out) / 10.0, 1.0) * 0.2,       # connectivity
        min(node.access_count / 5.0, 1.0) * 0.2,           # retrieval frequency
        min(node.cross_session_count / 3.0, 1.0) * 0.2,    # cross-session span
        (1.0 if node.conflicts_with else 0.0) * 0.1,       # conflict = needs attention
    ]
    return min(sum(signals), 1.0)
```

### 5.4 Source Node Integrity

**Problem**: `source_nodes` TEXT field ("id1,id2,id3") has no referential integrity. Source nodes can be deactivated or archived, leaving scene nodes pointing at ghosts.

**Solution**: Orphan detection in consolidation, not at write time (write path stays fast).

```
Reflector.consolidate(user_id):
    ...
    ├── N. Source node integrity check (scene nodes only)
    │
    │   For each active scene node:
    │     source_ids = scene.source_nodes.split(",")
    │     active_sources = [id for id in source_ids
    │                       if graph.nodes[id].is_active]
    │
    │     if len(active_sources) == 0:
    │       # All sources gone → scene is orphaned → deactivate
    │       deactivate(scene, reason="orphaned")
    │
    │     elif len(active_sources) < len(source_ids) * 0.5:
    │       # >50% sources gone → scene is weakening → reduce confidence
    │       scene.confidence *= 0.8
    │
    │     # Update source_nodes to only active ones
    │     scene.source_nodes = ",".join(active_sources)
    │
    └── ...
```

**Why not FK constraints**: Scene nodes reference 5-20 source nodes. FK checks on every write would be 5-20 lookups per scene creation. Batch validation in consolidation (runs daily) is much cheaper and catches the same issues.

### 5.5 Conflict Detection and Resolution

**Problem**: User says "I prefer Go" in session 1, then "Actually Python is better for data science" in session 5. Both become semantic nodes. Without conflict resolution, the agent holds contradictory beliefs.

**Current system**: `TypedObserver` already detects contradictions via L2_DISTANCE and supersedes the old memory. But this only works within a single `observe()` call — it misses cross-session contradictions that emerge gradually.

**Graph-level conflict detection** (runs in consolidation):

```
Reflector.consolidate(user_id):
    ...
    ├── M. Cross-session conflict detection
    │
    │   For each semantic node pair (A, B) where:
    │     - Both active
    │     - High association edge weight (> 0.7) = same topic
    │     - Low content similarity (cosine < 0.4) = different claims
    │     - Different sessions
    │
    │   → This is a potential contradiction.
    │
    │   Resolution strategy (automatic, no LLM):
    │
    │   1. RECENCY WINS (default):
    │      Newer node is "kept", older is "superseded"
    │      older.conflicts_with = newer.node_id
    │      older.conflict_resolution = 'superseded'
    │      older.confidence *= 0.5  (don't deactivate — may still be useful context)
    │      newer.conflict_resolution = 'kept'
    │      newer.confidence = min(newer.confidence + 0.1, 0.95)
    │
    │   2. CONFIDENCE WINS (when age difference < 7 days):
    │      If both are recent, higher confidence wins.
    │      Lower-confidence node gets superseded.
    │
    │   3. ESCALATE TO REFLECTION (when both high-confidence + recent):
    │      If both confidence > 0.7 and both < 7 days old:
    │      Mark both as conflict_resolution = 'pending'
    │      → Reflector.reflect() will synthesize a scene node
    │        that resolves the contradiction with nuance
    │        e.g., "User prefers Go generally but Python for data science"
    │
    └── ...
```

**Why not just supersede like the existing Observer?** The Observer's supersede is binary: old dies, new lives. But cross-session contradictions are often **nuanced** — the user's preference may be context-dependent. The graph approach preserves both nodes (with reduced confidence on the loser) and can synthesize a scene node that captures the nuance.

**Conflict in activation**: During spreading activation, conflicted nodes get a penalty:

```python
# In activation scoring (Phase 3 of §3.2):
if node.conflict_resolution == 'superseded':
    combined_score *= 0.5   # deprioritize but don't exclude
if node.conflict_resolution == 'pending':
    combined_score *= 0.7   # uncertain — reduce but keep visible
```

### 5.6 Adjacency List Format

```json
// edges_out format in memory_graph_nodes
[
  {"id": "n_abc123", "t": "temporal",    "w": 1.0},
  {"id": "n_def456", "t": "abstraction", "w": 0.8},
  {"id": "n_ghi789", "t": "causal",      "w": 1.5}
]
```

Short keys (`id`, `t`, `w`) to minimize JSON storage. Typical node has 2-5 edges → ~200 bytes of JSON.

**Bidirectional traversal**: For spreading activation, we need both outgoing AND incoming edges. Two options:

```
Option A: Store edges_in as well (doubles storage, simpler traversal)
Option B: Store only edges_out, build reverse index at load time

→ Choose Option A for nodes with < 50 edges (vast majority)
→ For hub nodes (> 50 edges), only store edges_out and use
  a SQL query for reverse lookup:
  SELECT node_id, edges_out FROM memory_graph_nodes
  WHERE user_id = :uid AND is_active = 1
  AND JSON_CONTAINS(edges_out, JSON_OBJECT('id', :target_id))
```

Pragmatic choice: start with Option A (both `edges_out` and `edges_in`). If storage becomes an issue, optimize later.

```sql
-- Add edges_in to schema
ALTER TABLE memory_graph_nodes ADD COLUMN edges_in JSON DEFAULT '[]';
```

**Edge count limits and pruning:**

```
MAX_EDGES_PER_NODE = 30

When adding an edge would exceed the limit:
  - Sort existing edges by weight (ascending)
  - Drop the lowest-weight edge to make room
  - This naturally prunes weak associations over time

Why 30: Synapse uses K=15 per direction. We use 30 for edges_out
(bidirectional stored separately). At ~20 bytes/edge JSON, 30 edges = 600 bytes.
Even at the cap, edges_out + edges_in < 1.2KB per node.
```

**Querying edges by type**: The JSON format doesn't support efficient "find all causal edges" queries. This is intentional — that query pattern only occurs in consolidation (batch, async), not in the retrieval hot path. For consolidation:

```python
# Python-side filtering on cached graph (fast, no DB query)
causal_edges = [e for e in node.edges_out if e.edge_type == "causal"]
```

**Concurrent edge updates**: Edge mutations only happen in two places:
1. `GraphBuilder.ingest()` — appends edges to NEW nodes (no contention, node doesn't exist yet) and updates EXISTING neighbors' `edges_in` (async, batched)
2. `Reflector.consolidate()` — batch weight updates (single writer, runs in governance scheduler)

Neither path has concurrent writers to the same node. The governance scheduler is single-threaded per user. Ingest's async neighbor updates use `UPDATE ... WHERE node_id = :id AND is_active = 1` — if a consolidation deactivated the node between ingest and async update, the UPDATE is a no-op (correct behavior).

### 5.7 Write Path Optimization

**Problem**: Each memory creation touches multiple rows (node + neighbor edge updates).

**Solution**: Batch writes with deferred edge updates.

```
Per-turn write path (latency-critical):

  1. INSERT new nodes (episodic + semantic)     -- 1 batch INSERT
  2. SET edges_out on new nodes                 -- included in step 1
     (temporal edge to prev episodic, abstraction edges)
  3. DONE. Return to caller.                    -- total: 1 SQL statement

  Deferred (async, <100ms after response):
  4. UPDATE neighbor nodes' edges_in            -- 1 batch UPDATE
  5. Find and create association edges           -- 1 vector search + 1 batch UPDATE
     (top-5 similar semantic nodes)

Total per-turn SQL: 1 INSERT (sync) + 2 UPDATEs (async)
vs tabular: 1 INSERT nodes + N INSERT edges + FK checks = N+1 statements
```

**Consolidation write path** (async, non-blocking):

```
Batch operations, not per-node:

  1. Merge duplicates:
     UPDATE memory_graph_nodes SET is_active = 0
     WHERE node_id IN (:duplicate_ids)           -- 1 batch UPDATE

  2. Strengthen/decay edge weights:
     -- Collect all weight changes in Python
     -- Apply as batch UPDATE per node
     UPDATE memory_graph_nodes SET edges_out = :new_edges
     WHERE node_id IN (:changed_ids)             -- 1 batch UPDATE

  3. Create scene nodes:
     INSERT INTO memory_graph_nodes ...           -- 1 batch INSERT
```

### 5.8 Tiered Graph Loading

Full graph load works for small/medium graphs but breaks at scale. We use a **three-tier loading strategy** that adapts to graph size:

```
┌─────────────────────────────────────────────────────────────────┐
│                    GRAPH SIZE TIERS                              │
│                                                                 │
│  Tier 1: Small graph (< 10K nodes)                              │
│    → Full load into memory, cache for session                   │
│    → Covers ~95% of users in year 1                             │
│                                                                 │
│  Tier 2: Medium graph (10K - 50K nodes)                         │
│    → Skeleton load (nodes without embeddings) + lazy embedding  │
│    → Activation runs on skeleton; embeddings fetched on-demand  │
│                                                                 │
│  Tier 3: Large graph (> 50K nodes)                              │
│    → Anchor-expand: load only 2-hop neighborhood of anchors     │
│    → Never load full graph; iterative expansion if needed       │
└─────────────────────────────────────────────────────────────────┘
```

**Tier 1: Full Load (< 10K nodes, ~5MB)**

```
1. SELECT all active nodes (single query)
2. Build in-memory adjacency dict
3. Cache for session duration (evict after 5 min idle)
4. Activation runs entirely in-memory, 0 DB queries

Memory: ~0.5KB/node (without embedding) + ~6KB/node (with embedding)
  10K nodes with embeddings: ~65MB
  10K nodes skeleton only: ~5MB
```

**Tier 2: Skeleton + Lazy Embedding (10K - 50K nodes)**

The key insight: spreading activation only needs **edges and confidence** to propagate. Embeddings are only needed for the initial anchor selection (dual trigger) and final scoring. So we load the graph structure without embeddings, and fetch embeddings only for the ~100 nodes that matter.

```
1. SKELETON LOAD (single query, ~0.5KB/node):
   SELECT node_id, node_type, confidence, trust_tier,
          edges_out, edges_in, importance, session_id
   FROM memory_graph_nodes
   WHERE user_id = :uid AND is_active = 1
   -- NOTE: no content, no embedding columns

   50K nodes × 0.5KB = ~25MB. Acceptable.

2. ANCHOR SELECTION (uses DB-side indexes, not in-memory):
   -- BM25 trigger: uses fulltext index
   SELECT node_id, L2_DISTANCE(embedding, :q_embed) AS dist
   FROM memory_graph_nodes
   WHERE user_id = :uid AND is_active = 1
   ORDER BY dist ASC LIMIT 20
   -- Vector trigger: uses IVF-flat index

3. ACTIVATE on skeleton (in-memory, no embeddings needed):
   Propagation uses edges + confidence + edge weights only.
   → Same algorithm, same 3 iterations, same lateral inhibition.

4. FETCH content + embeddings for top-K activated nodes only:
   SELECT node_id, content, embedding
   FROM memory_graph_nodes
   WHERE node_id IN (:top_k_ids)
   -- Typically 30-50 nodes. Tiny query.

5. SCORE using fetched embeddings + activation values.
```

**Why this works**: Spreading activation is a graph algorithm — it propagates along edges weighted by confidence and edge type. It does NOT use embeddings during propagation. Embeddings are only used at two points: (a) initial anchor selection (done DB-side via index), (b) final scoring (done on ~30 nodes). So we can run activation on a 50K-node skeleton that fits in 25MB.

**Tier 3: Anchor-Expand (> 50K nodes)**

For very large graphs, even the skeleton doesn't fit comfortably. We never load the full graph — instead, we expand outward from anchors.

```
1. ANCHOR SELECTION (same as Tier 2, DB-side):
   Get top-20 anchors via BM25 + vector search.

2. EXPAND HOP 1:
   SELECT node_id, node_type, confidence, trust_tier,
          edges_out, edges_in, importance
   FROM memory_graph_nodes
   WHERE node_id IN (:anchor_neighbor_ids) AND is_active = 1

   Anchor neighbors extracted from anchors' edges_out/edges_in.
   Typically 20 anchors × 5 avg edges = ~100 nodes.

3. EXPAND HOP 2:
   Same pattern. ~100 nodes × 5 edges = ~500 nodes.

4. EXPAND HOP 3 (if needed):
   ~500 × 5 = ~2,500 nodes. Cap here.

5. ACTIVATE on this local subgraph (~500-2,500 nodes):
   Same algorithm. Working set is small regardless of total graph size.

6. FETCH content + embeddings for top-K.
```

**Trade-off**: Tier 3 may miss globally important nodes that aren't within 3 hops of any anchor. This is acceptable because:
- If a node is important but unreachable from the query's anchors, it's probably not relevant to this query
- Scene nodes (high-level insights) tend to have high connectivity and are usually within 2 hops
- Worst case: falls back to pure vector search for the missed nodes (existing retriever path)

**Tier selection logic:**

```python
def _select_tier(self, user_id: str) -> int:
    count = self._get_node_count(user_id)  # cached, updated on ingest
    if count < 10_000:
        return 1  # full load
    if count < 50_000:
        return 2  # skeleton + lazy embedding
    return 3      # anchor-expand
```

### 5.9 Memory Budget

**Per-node memory breakdown:**

```
Skeleton (no embedding, no content):
  node_id(32) + type(1) + confidence(4) + trust_tier(2) +
  edges_out(~200) + edges_in(~200) + importance(4) + session_id(64) +
  overhead(~50)
  ≈ 560 bytes/node

With content (avg 200 chars):
  skeleton + content(200)
  ≈ 760 bytes/node

With embedding (1536 × 4 bytes):
  skeleton + content + embedding(6144)
  ≈ 6.9 KB/node
```

| Tier | Nodes | What's Loaded | Per-User Memory | 100 Concurrent |
|---|---|---|---|---|
| 1 skeleton | < 10K | skeleton + content (no embeddings) | 10K × 760B = **7.6MB** | 760MB |
| 2 skeleton | 10K-50K | skeleton only (no content, no embeddings) | 50K × 560B = **28MB** | 2.8GB |
| 3 anchor-expand | > 50K | ~2,500 node subgraph with content | 2.5K × 760B = **1.9MB** | 190MB |

**Embeddings are NEVER bulk-loaded into memory.** They're fetched on-demand for top-K nodes only (30 nodes × 6.9KB = 207KB). Anchor selection uses DB-side IVF-flat index.

**Eviction policy (LRU with hard cap):**

```python
class GraphCache:
    """Per-user LRU graph cache with global memory cap."""

    MAX_MEMORY_BYTES = 512 * 1024 * 1024  # 512MB hard cap
    IDLE_TTL_SECONDS = 300                 # evict after 5 min idle

    def get(self, user_id: str) -> InMemoryGraph | None:
        entry = self._cache.get(user_id)
        if entry and (now() - entry.last_access) > self.IDLE_TTL_SECONDS:
            self._evict(user_id)
            return None
        if entry:
            entry.last_access = now()
        return entry.graph if entry else None

    def put(self, user_id: str, graph: InMemoryGraph) -> None:
        size = graph.estimated_bytes()
        # Evict LRU entries until we have room
        while self._total_bytes + size > self.MAX_MEMORY_BYTES:
            lru_user = min(self._cache, key=lambda u: self._cache[u].last_access)
            self._evict(lru_user)
        self._cache[user_id] = CacheEntry(graph=graph, last_access=now(), size=size)
        self._total_bytes += size
```

**Worst case**: 512MB cap ÷ 28MB (Tier 2 max) = 18 concurrent heavy users with cached graphs. Remaining users fall through to DB queries (still fast — single SELECT). This is acceptable: most users are Tier 1 (7.6MB), so 512MB supports ~67 concurrent cached users.

### 5.10 Relationship to Existing Tables

```
memory_graph_nodes                    Existing Tables
┌──────────────────┐                 ┌──────────────────┐
│ episodic node    │───event_id────▶│ agent_events     │
│ (denormalized    │                 │ (source of truth)│
│  content copy)   │                 │                  │
├──────────────────┤                 ├──────────────────┤
│ semantic node    │───memory_id───▶│ memories         │
│ (denormalized    │                 │ (source of truth)│
│  content copy)   │                 │                  │
├──────────────────┤                 ├──────────────────┤
│ scene node       │                 │ (graph-native,   │
│ (self-contained) │                 │  no source table)│
└──────────────────┘                 └──────────────────┘
```

Content is **denormalized** (copied into graph node) to avoid JOINs during retrieval. Source tables remain the source of truth. Consistency:
- New memories → `GraphBuilder.ingest()` creates graph node (sync)
- Memory deactivated → `GraphBuilder.sync()` deactivates graph node (async, in consolidation)
- Content divergence is acceptable — graph node content is a snapshot at creation time

### 5.11 Storage Estimates

| Component | Per User (1 year) | Storage |
|---|---|---|
| Episodic nodes (with edges_out/in) | ~5,000 | ~5MB |
| Semantic nodes (with embeddings + edges) | ~2,000 | ~10MB |
| Scene nodes | ~200 | ~1MB |
| **Total per user** | ~7,200 nodes | **~16MB** |
| **1,000 users** | | **~16GB** |

Slightly more than tabular (~12MB) due to denormalized content and bidirectional edges, but eliminates all JOINs. Net win.

### 5.12 Query Patterns and Complexity

| Operation | tabular (separate edge table) | graph (adjacency in node) |
|---|---|---|
| 1-hop neighbors | `SELECT + JOIN edges` | `JSON parse edges_out` (in-memory) |
| 3-hop traversal | 3× `SELECT + JOIN` = 9 JOINs | 3× batch `SELECT` or fully in-memory |
| Spreading activation (3 iter) | 9+ DB round-trips | 0 DB queries (in-memory cache) |
| Add edge | `INSERT edge + FK check` | `UPDATE source.edges_out` (append to JSON) |
| Find if already reflected | `JSON_CONTAINS(source_node_ids, ...)` | `source_nodes LIKE '%id%'` (TEXT index) |
| Full graph load | `SELECT nodes + SELECT edges + JOIN` | `SELECT nodes` (single table, edges included) |

Manageable. MatrixOne handles this easily with IVF-flat indexing.

---

## 6. Module Design

### 6.1 New Modules

```
core/memory/
├── graph.py              # MemoryGraph: single-table CRUD + in-memory graph cache
├── graph_builder.py      # GraphBuilder: incremental graph construction (batch writes)
├── activation.py         # SpreadingActivation: in-memory activation propagation
├── reflector.py          # Reflector: consolidate + reflect + opinion evolution
└── importance.py         # ImportanceScorer: multi-signal importance scoring

api/models/
└── memory_graph.py       # MemoryGraphNode ORM model (single table)
```

### 6.2 Key Interfaces

```python
class MemoryGraph(DbConsumer):
    """Single-table graph with in-memory cache for activation."""

    # -- Persistence (DB) --
    def add_nodes(self, nodes: list[GraphNode]) -> None:
        """Batch insert nodes (with edges_out/edges_in pre-populated)."""
    def update_edges(self, updates: dict[str, list[Edge]]) -> None:
        """Batch update edges_out/edges_in for multiple nodes."""
    def deactivate_nodes(self, node_ids: list[str],
                         superseded_by: str | None = None) -> None: ...
    def find_similar_nodes(self, embedding: list[float], user_id: str,
                           node_type: str | None = None,
                           threshold: float = 0.7, limit: int = 10) -> list[GraphNode]: ...

    # -- In-memory cache --
    def load(self, user_id: str) -> InMemoryGraph:
        """Load full user graph into memory. Cached per session."""
    def invalidate(self, user_id: str) -> None:
        """Invalidate cache (after consolidation or bulk writes)."""


class InMemoryGraph:
    """In-memory adjacency graph for fast activation traversal.
    Built from single SELECT, no JOINs. All traversal is dict lookup."""

    nodes: dict[str, GraphNode]           # node_id → node
    out_edges: dict[str, list[Edge]]      # node_id → outgoing edges
    in_edges: dict[str, list[Edge]]       # node_id → incoming edges

    def neighbors(self, node_id: str,
                  direction: str = "both") -> list[tuple[GraphNode, Edge]]: ...
    def subgraph(self, node_ids: list[str], max_hops: int = 2) -> InMemoryGraph: ...


class GraphBuilder(DbConsumer):
    """Incremental graph construction. Sync write + async edge updates."""

    def ingest(self, user_id: str, new_memories: list[Memory],
               source_events: list[dict]) -> GraphBuildResult:
        """Sync: INSERT new nodes. Async: UPDATE neighbor edges."""
    def sync(self, user_id: str) -> None:
        """Propagate deactivations from memories/events to graph nodes."""


class SpreadingActivation:
    """Activation propagation on InMemoryGraph. Stateless, no DB access."""

    def activate(self, graph: InMemoryGraph,
                 query_embedding: list[float], query_text: str,
                 iterations: int = 3, top_k: int = 30) -> list[ActivatedNode]: ...


class Reflector(DbConsumer):
    """Three-phase reflection: consolidate + reflect + evolve."""

    def consolidate(self, user_id: str) -> ConsolidateResult: ...
    def reflect(self, user_id: str, *, days: int = 1) -> list[GraphNode]: ...
    def evolve_opinions(self, user_id: str, new_event: dict) -> list[OpinionUpdate]: ...


class ImportanceScorer:
    """Multi-signal importance scoring. Stateless, no LLM."""

    def score_component(self, component: list[GraphNode]) -> float: ...
    def score_event(self, event: dict, graph: InMemoryGraph) -> float: ...
```

### 6.3 Data Structures

```python
class NodeType(str, Enum):
    EPISODIC = "episodic"
    SEMANTIC = "semantic"
    SCENE = "scene"

class EdgeType(str, Enum):
    TEMPORAL = "temporal"
    ABSTRACTION = "abstraction"
    ASSOCIATION = "association"
    CAUSAL = "causal"
    CONSOLIDATION = "consolidation"

# Edge type multipliers for spreading activation
EDGE_TYPE_BOOST: dict[EdgeType, float] = {
    EdgeType.TEMPORAL: 1.0,
    EdgeType.ABSTRACTION: 0.8,
    EdgeType.ASSOCIATION: 1.0,  # weight is already cosine_sim
    EdgeType.CAUSAL: 1.5,      # causal links are high-value
    EdgeType.CONSOLIDATION: 1.0,
}

@dataclass
class Edge:
    """Lightweight edge stored in node's edges_out/edges_in JSON."""
    target_id: str
    edge_type: EdgeType
    weight: float = 1.0

@dataclass
class GraphNode:
    node_id: str
    user_id: str
    node_type: NodeType
    content: str
    embedding: list[float] | None = None
    event_id: str | None = None       # episodic reference
    memory_id: str | None = None      # semantic reference
    session_id: str | None = None
    confidence: float = 0.75
    trust_tier: str = "T3"
    edges_out: list[Edge] = field(default_factory=list)
    edges_in: list[Edge] = field(default_factory=list)
    source_nodes: str | None = None   # scene only: "id1,id2,id3"
    importance: float = 0.0
    is_active: bool = True

@dataclass
class ActivatedNode:
    node: GraphNode
    activation: float          # final activation energy
    semantic_score: float      # cosine similarity to query
    combined_score: float      # weighted final score
    path: list[str] | None = None  # activation path for explainability
```

---

## 7. MatrixOne-Specific Optimizations

### 7.1 Graph Load Query (Hot Path)

The single most important query — loads the entire user graph into memory for activation:

```sql
-- Single query, no JOINs, returns everything needed for in-memory activation
SELECT node_id, node_type, content, embedding, confidence, trust_tier,
       edges_out, edges_in, importance, session_id, source_nodes
FROM memory_graph_nodes
WHERE user_id = :uid AND is_active = 1;
```

For 7,000 nodes this returns ~3MB. MatrixOne serves this in <20ms from the composite index `idx_user_active`.

### 7.2 Dual Trigger Queries (Anchor Selection)

When the graph is NOT cached (first query in session), we can skip full graph load and do targeted anchor selection:

```sql
-- Lexical trigger (BM25 via MatrixOne fulltext)
SELECT node_id, content,
       MATCH(content) AGAINST(:query IN BOOLEAN MODE) AS bm25_score
FROM memory_graph_nodes
WHERE user_id = :uid AND is_active = 1
ORDER BY bm25_score DESC LIMIT 20;

-- Semantic trigger (vector via MatrixOne IVF-flat)
SELECT node_id, content, L2_DISTANCE(embedding, :q_embed) AS dist
FROM memory_graph_nodes
WHERE user_id = :uid AND is_active = 1
ORDER BY dist ASC LIMIT 20;

-- Then load only the 2-hop neighborhood of anchors
SELECT node_id, node_type, content, embedding, confidence,
       trust_tier, edges_out, edges_in, importance, session_id
FROM memory_graph_nodes
WHERE user_id = :uid AND is_active = 1
  AND node_id IN (:anchor_ids_and_neighbor_ids);
```

This "lazy load" path fetches ~200-500 nodes instead of 7,000 — useful for cold start or very large graphs.

### 7.3 Time-Travel for Debugging

MatrixOne's snapshot capability enables graph state reconstruction:

```sql
-- "What did the agent's memory graph look like on March 1st?"
SELECT * FROM memory_graph_nodes {MO_TS = '2026-03-01 00:00:00'}
WHERE user_id = :uid AND is_active = 1;

-- "What scene nodes existed when this decision was made?"
SELECT * FROM memory_graph_nodes {MO_TS = :decision_timestamp}
WHERE user_id = :uid AND node_type = 'scene' AND is_active = 1;
```

Critical for auditing: reconstruct the exact graph state the agent saw, including which scene nodes (reflections) influenced the response.

### 7.4 Zero-Copy Branching for Reflection Testing

Before deploying a reflection result, validate it in a sandbox:

```
1. Create branch: CREATE DATABASE reflection_test FROM main
2. Apply scene nodes to branch
3. Run test queries against branched graph
4. Compare retrieval quality: branched vs main
5. If quality improves: merge to main
6. If quality degrades: discard branch
```

This uses MatrixOne's zero-copy branching — no data duplication, instant creation.

---

## 8. Comparison with Prior Art

### 8.1 What We Take from Each System

| System | What We Adopt | What We Don't | Why |
|---|---|---|---|
| **Synapse** | Spreading activation, lateral inhibition, fan effect, dual trigger | PageRank as global prior | Our confidence decay serves the same role as PageRank (structural importance) |
| **EverMemOS** | MemCell→MemScene two-level consolidation, engram lifecycle | Foresight signals (predictive memory) | Adds complexity without clear ROI for dev agent use case |
| **Hindsight** | Retain/Recall/Reflect separation, opinion evolution with confidence | 4-network separation (world/experience/opinion/observation) | Our 3-layer graph (episodic/semantic/scene) is simpler and maps to existing tables |
| **A-Mem** | Self-organizing links (association edges built automatically) | Zettelkasten note-taking metaphor | Our graph structure is more principled (typed edges with activation dynamics) |
| **Generative Agents** | Importance scoring, reflection triggers | Multi-level reflection (reflecting on reflections) | Single-level reflection sufficient for dev agent; multi-level has diminishing returns |

### 8.2 What's Novel in Our Design

1. **MatrixOne-native graph**: Vector search, fulltext search, time-travel, and zero-copy branching all in one DB — no external graph DB, no Elasticsearch, no Pinecone
2. **Existing audit trail integration**: Scene nodes link back to `agent_events` via `source_node_ids` → full provenance chain → time-travel to any past graph state
3. **Gradual migration**: Activation retrieval coexists with existing hybrid retrieval — no big-bang cutover
4. **Opinion evolution tied to governance**: Scene confidence decay uses the same trust tier system as existing memories — GovernanceScheduler handles both uniformly

### 8.3 What We Explicitly Don't Do

| Feature | Why Not |
|---|---|
| **Multi-level reflection** (reflecting on reflections) | Diminishing returns. Generative Agents used this for 25-agent social simulation — we have single-user dev agents. One level of reflection is sufficient. |
| **Social reflection** (learning from other agents' experiences) | Architecture decision: memory is per-user, not per-agent. All agents serving the same user already share the same memory pool. No cross-user learning by design (privacy). |
| **Separate opinion memory type** | Scene nodes with confidence evolution serve the same purpose. Adding a 4th node type increases complexity without clear benefit. |
| **In-database activation propagation** | MatrixOne SQL can't express lateral inhibition + sigmoid activation efficiently. Python-side propagation on a small working set (<1000 nodes) is fast enough (<30ms). Future optimization if needed. |
| **Real-time graph updates during streaming** | Graph building is post-turn, not mid-stream. Streaming chunks go to sensory buffer; graph is updated after the turn completes. Simpler, no race conditions. |

---

## 9. Implementation Plan

> **tabular/graph coexistence**: This system is implemented as `graph` under `core/memory/graph/`, completely independent from the existing `tabular` (`core/memory/tabular/`). Both implement the same `MemoryReader`/`MemoryWriter`/`MemoryAdmin` protocols. A factory selects which version to instantiate. See [backend-coexistence.md](backend-coexistence.md) for the full design.

### Phase 1: Foundation (Week 1-2)

**Goal**: tabular/graph split + graph data model + basic graph building.

```
1. Move current memory code to core/memory/tabular/, add factory (pure refactor, all tests pass)
2. Create core/memory/graph/ skeleton with GraphMemoryService implementing protocols
3. Create memory_graph_nodes table via migration
4. Implement GraphStore (CRUD + neighbor queries)
5. Implement GraphBuilder.ingest()
6. Tests: factory routing, graph building, protocol compliance for both versions
```

**Deliverable**: tabular/graph factory works. Every new memory (via graph) automatically creates graph nodes and edges. No retrieval changes yet.

### Phase 2: Activation Retrieval (Week 3-4)

**Goal**: Spreading activation retrieval path.

```
1. Implement SpreadingActivation engine
2. Implement ActivationRetriever (wraps SpreadingActivation + scoring)
3. Add activation_retrieve() to MemoryRetriever (alongside existing path)
4. Implement fallback logic (graph available → activation; else → legacy)
5. Tests: activation propagation correctness, multi-hop retrieval,
   lateral inhibition suppression, fallback behavior
```

**Deliverable**: Queries can use activation-based retrieval. A/B testable against existing retrieval.

### Phase 3: Reflection (Week 5-6)

**Goal**: Consolidation + reflection + opinion evolution.

```
1. Implement ImportanceScorer
2. Implement Reflector.consolidate() (merge, strengthen, detect contradictions)
3. Implement Reflector.reflect() (subgraph identification, LLM synthesis, scene creation)
4. Implement Reflector.evolve_opinions() (evidence alignment, confidence update)
5. Wire into GovernanceScheduler.run_daily()
6. Tests: consolidation correctness, reflection quality, opinion evolution,
   importance scoring calibration
```

**Deliverable**: Agent reflects daily, creates scene nodes, evolves beliefs.

### Phase 4: Integration & Evaluation (Week 7-8)

**Goal**: End-to-end validation and tuning.

```
1. Backfill existing memories into graph (migration script)
2. A/B test: activation retrieval vs legacy retrieval on golden sessions
3. Tune hyperparameters: spreading factor, inhibition strength, importance thresholds
4. Integrate reflection signals into InputFaceLearner
5. Documentation and monitoring dashboards
```

**Deliverable**: Production-ready system with measured quality improvement.

---

## 10. Risks and Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Graph grows unbounded | Memory/query degradation | Tiered loading (§5.8): skeleton load for 10K-50K, anchor-expand for 50K+. Node archival for dormant nodes. No hard cap needed — tiers adapt automatically |
| Tier 3 anchor-expand misses distant scenes | Important but topologically distant scenes invisible | Mitigation: anchor selection includes top-5 scene nodes by importance regardless of activation proximity. Consolidation periodically tags "global anchor" scenes (importance >0.8, cross-session span >3) that are always included in Tier 3 working set |
| Reflection produces low-quality scenes | Bad memories pollute future retrieval | Conservative initial confidence (T4, 0.5), opinion evolution can quarantine bad scenes. A/B test plan: Phase 4 runs reflection prompt variants on 20% of users, measures scene usefulness rate (target >70%). Prompt managed by PromptOptimizer with `prompt_template_id = "reflection_synthesis"` |
| Complexity jump (2→5 modules) | Onboarding/debug cost increases | Mitigation: (1) each module has a single-file entry point with <200 LOC public API, (2) legacy retriever remains as permanent fallback — new modules are additive, not replacement, (3) `mo-agent graph inspect` CLI command for debugging, (4) incremental rollout: Phase 1-2 run shadow-mode (log but don't serve) before Phase 3-4 go live |
| Activation propagation too slow | Retrieval latency regression | Cap iterations at 3. Tier 2/3 limit working set to <2,500 nodes regardless of total graph size. Fallback to legacy retrieval |
| Cold start (new users have sparse graph) | Activation provides no benefit over vector search | Fallback to legacy retrieval when graph has < 50 nodes |
| LLM reflection calls add cost | $/user/day increase | Importance threshold filters ~80% of candidates; ~0.4 scenes/day/user = ~1 LLM call/day |
| MatrixOne vector index performance at scale | Query latency at >10K nodes | IVF-flat with LISTS=100 handles 10K vectors in <10ms; monitor and adjust |

---

## 11. Success Metrics

| Metric | Current Baseline | Target | How to Measure |
|---|---|---|---|
| Multi-hop retrieval accuracy | Not measured (no multi-hop test set) | Establish baseline, then +20% | Create golden set of multi-hop queries from real sessions |
| Retrieval relevance (human eval) | ~70% (estimated) | 85% | Sample 100 retrievals/week, human rate relevance |
| Cross-session pattern detection | 0 (no reflection) | >1 scene/user/week for active users | Count scene nodes created |
| Scene quality (human eval) | N/A | >70% rated "useful" | Sample scenes, human rate usefulness |
| Retrieval latency p95 | ~40ms | <80ms | Monitor activation retrieval path |
| Memory token usage | ~800 tokens/turn | ~400 tokens/turn (activation is more selective) | Measure tokens in memory section of prompt |
| False memory rate | Not measured | <5% of scenes quarantined within 7 days | Track scene quarantine rate |

**Validation method — session replay**: Replay 50 historical sessions (diverse intents) with activation retrieval in shadow mode. For each turn, compare retrieved memory set (activation) vs baseline (vector-only). Human-rate which set is more relevant. This produces the multi-hop baseline and validates the +20% target before going live.

**Hyperparameter sensitivity plan**: Phase 4 includes grid search over key parameters using the replay corpus:

| Parameter | Default | Search Range | Sensitivity |
|---|---|---|---|
| S (similarity threshold) | 0.8 | 0.6–0.9 | High — controls activation spread radius |
| β (lateral inhibition) | 0.15 | 0.05–0.25 | Medium — affects node competition |
| γ (importance boost) | 5.0 | 2.0–10.0 | Low — scales importance signal |
| MAX_ITERATIONS | 3 | 2–5 | High — latency vs recall tradeoff |
| IMPORTANCE_REFLECT_THRESHOLD | 0.7 | 0.5–0.9 | Medium — controls reflection frequency |

Sensitivity classification (High/Medium/Low) will be confirmed empirically. Parameters classified "High" get per-user adaptive tuning in a future iteration.

---

## 12. Open Questions

1. **Embedding model**: Current system uses configurable embedding provider. Should graph nodes use the same embeddings as memories, or a specialized smaller model (like all-MiniLM-L6-graph used by Synapse) for faster graph operations?

2. **Cross-user patterns**: Current design is strictly per-user. Should we ever allow opt-in cross-user pattern sharing (e.g., "80% of users hit this same CI issue")? Privacy implications are significant.

3. **Graph visualization**: Should we expose graph structure in the CLI/API for debugging? (`mo-agent graph show --user alice --depth 2`)

4. **Activation caching**: Should we cache activation state between turns within a session? (Synapse caches and updates only during consolidation windows.)

5. **Reflection prompt evolution**: The reflection prompt is critical for scene quality. Should it be managed by the existing PromptOptimizer (auto-evolving) or hand-tuned?

---

## 13. Integration with Intent-Driven Memory Loading

The [Intent Unification system](intent-driven-loading.md) classifies each turn into a `task_type` (e.g., CODE_REVIEW, DEBUG, PREFERENCE) and a `memory_mode` (NONE / L0_CORE / COMPRESSED / FULL). This directly influences the graph memory system:

### 13.1 Task Type → Activation Parameters

```python
# Intent-aware edge type boosting for spreading activation
TASK_EDGE_BOOST: dict[str, dict[str, float]] = {
    "CODE_REVIEW": {"causal": 1.5, "temporal": 0.5, "similarity": 1.0},
    "DEBUG":       {"causal": 2.0, "temporal": 1.5, "similarity": 0.5},
    "PREFERENCE":  {"similarity": 1.5, "refinement": 1.5, "causal": 0.5},
    "GENERAL":     {},  # use defaults
}
```

When `RouteDecision.task_type` is available, `SpreadingActivation` applies these boosts to `edge_type_weights` before propagation. This means a DEBUG query preferentially follows causal chains, while a PREFERENCE query follows similarity/refinement edges.

### 13.2 Memory Mode → Graph Loading

| Memory Mode | Graph Behavior |
|---|---|
| NONE | Skip graph entirely (command/feedback turns) |
| L0_CORE | Load only scene nodes (profile-like insights) — no activation |
| COMPRESSED | Activate with MAX_ITERATIONS=2, top-K=5 |
| FULL | Full activation (MAX_ITERATIONS=3, top-K=10) — fallback mode |

### 13.3 Importance Weights by Task Type

Task type can bias importance scoring during consolidation:

```python
# Contradiction/correction signal weighted higher for DEBUG sessions
TASK_IMPORTANCE_WEIGHTS: dict[str, dict[str, float]] = {
    "DEBUG":      {"contradiction": 0.45, "recurrence": 0.25, "centrality": 0.15, "cross_session": 0.15},
    "CODE_REVIEW": {"cross_session": 0.35, "centrality": 0.30, "recurrence": 0.20, "contradiction": 0.15},
    "DEFAULT":    {"contradiction": 0.30, "cross_session": 0.25, "centrality": 0.25, "recurrence": 0.20},
}
```

This integration is implemented in Phase 4 (Week 7-8) after both systems are stable independently.

---

## References

1. Jiang, H. et al. (2026). "Synapse: Empowering LLM Agents with Episodic-Semantic Memory via Spreading Activation." arXiv:2601.02744. — Spreading activation on unified episodic-semantic graph, LoCoMo SOTA.

2. Hu, C. et al. (2026). "EverMemOS: A Self-Organizing Memory Operating System for Structured Long-Horizon Reasoning." arXiv:2601.02163. — MemCell→MemScene consolidation, engram lifecycle, LoCoMo 92.3%.

3. Latimer, C. et al. (2025). "Hindsight: Building Agent Memory that Retains, Recalls, and Reflects." arXiv:2512.12818. — Four-network memory (world/experience/opinion/observation), Retain/Recall/Reflect, LongMemEval 91.4%.

4. Xu, W. et al. (2025). "A-Mem: Agentic Memory for LLM Agents." arXiv:2502.12110. — Zettelkasten-inspired self-organizing memory with agent-driven linking.

5. Park, J.S. et al. (2023). "Generative Agents: Interactive Simulacra of Human Behavior." — Importance scoring, multi-level reflection, foundational work on agent memory.

6. Collins, A.M. & Loftus, E.F. (1975). "A Spreading-Activation Theory of Semantic Processing." Psychological Review. — Original spreading activation theory.

7. Anderson, J.R. (1983). "A Spreading Activation Theory of Memory." Journal of Verbal Learning and Verbal Behavior. — ACT-R framework, fan effect.
