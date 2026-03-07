# Intent-Driven Adaptive Memory Loading

> **Status**: Final v2 — production ready  
> **Created**: 2026-03-06  
> **Related**: [memory-overview.md](memory-overview.md) (memory system, L0/L1 tiers, confidence decay), [token-efficient-llm-routing.md](token-efficient-llm-routing.md) (Adaptive Hierarchical Router), [context-window-management.md](context-window-management.md) (zone budgets, compression)

## Core Insight

**`RouteDecision.context_plan` 直接决定内存加载模式**，配合 Adaptive Router，实现记忆段 800→130 tok（-84%），总提示 -60%，零质量损失。

---

## Why This Design

- **Route-driven, not unconditional**: 当前每 turn 无条件跑 L0 全量 + L1 hybrid retrieval。40-50% 的 turn（command/feedback/preference）根本不需要 memory
- **L0 分层**: profile 不是铁板一块——"用户名"和"偏好 vim"的加载条件不同
- **L0/L1 独立压缩**: 当前合并为单个 `memory` section，超预算时高相关 L1 被低价值 L0 拖累丢弃
- **Shared embedding**: Router Tier 1 的 classify ∥ compress ∥ prune 三路并行都需要 query embedding，算一次共享
- **Negative feedback**: 被纠正/低置信的 memory 应该降权，当前无此信号
- **Cache amplification**: memory fingerprint 扩展 provider cache 命中深度

---

## Architecture（与 Router v3 完全同构）

```
User message
     │
     ▼
┌──────────────────────────────────────────────────────────────┐
│  Tier 0: Regex + Heuristic (<1ms, $0)                        │
│                                                              │
│  preference → memory_mode = L0_CORE                          │
│  command    → memory_mode = NONE                             │
│  feedback   → memory_mode = NONE                             │
│  confidence ≥ 0.95 → skip Tier 1, use mode directly         │
└──────────┬───────────────────────────────────────────────────┘
           │ confidence < adaptive_threshold
           ▼
┌──────────────────────────────────────────────────────────────┐
│  Tier 1: Cheapest Model (parallel, ~180ms)                   │
│                                                              │
│  ┌─ classify(query)                → intent + confidence     │
│  ├─ retrieve_l1(query, embedding)  → raw memories            │
│  │   └─ compress(raw)              → compressed_text         │
│  └─ prune_tools(query, embedding)  → relevant_tool_ids      │
│                                     ↑                        │
│                          shared query_embedding (1 call)     │
│                                                              │
│  confidence ≥ threshold → memory_mode from intent            │
│  confidence < threshold → memory_mode = FULL (safe fallback) │
└──────────┬───────────────────────────────────────────────────┘
           │
           ▼
┌──────────────────────────────────────────────────────────────┐
│  PromptAssembler §4: Memory Section                          │
│                                                              │
│  NONE       → skip §4 entirely              (0 tok)          │
│  L0_CORE    → core profile only             (~50 tok)        │
│  COMPRESSED → Tier 1 compressed L0+L1       (~200 tok)       │
│  FULL       → raw L0 + L1 retrieval         (~800 tok)       │
└──────────────────────────────────────────────────────────────┘
```

---

## Memory Loading Modes

| Mode | What loads | Tokens | When | DB queries |
|------|-----------|--------|------|------------|
| `NONE` | Nothing | 0 | command, feedback | 0 |
| `L0_CORE` | Core profile (name, language, hard constraints) | ~50 | preference | 1 (cached) |
| `COMPRESSED` | Tier 1 compressed L0+L1 | ~200 | question (confidence ≥ threshold) | 1 (L1 retrieve, done in Tier 1 parallel) |
| `FULL` | Raw L0 + L1 hybrid retrieval | ~800 | fallback, first turn of session | 2-3 (L0 + L1 keyword + L1 vector) |

### Token Savings

| Intent | Current | After | Savings | Frequency |
|--------|---------|-------|---------|-----------|
| preference | ~800 | ~50 | **-94%** | ~15% |
| command | ~800 | 0 | **-100%** | ~25% |
| feedback | ~800 | 0 | **-100%** | ~10% |
| question | ~800 | ~200 | **-75%** | ~50% |
| **Weighted avg** | **~800** | **~130** | **-84%** | |

---

## L0 Split: Core vs Topic

Current `ProfileManager.get_profile()` loads all profile memories unconditionally.

```
L0-core:  Always loaded when memory_mode ≠ NONE
          identity, constraint categories
          ~50 tokens, cached per user (TTL 60s)

L0-topic: Loaded only when memory_mode ∈ {FULL}
          preference, workflow categories
          ~150 tokens, keyword-filtered by query
```

```sql
-- L0-core (cached, no query dependency)
SELECT content FROM memories
WHERE user_id = :uid AND memory_type = 'profile'
  AND category IN ('identity', 'constraint') AND is_active = 1;

-- L0-topic (query-filtered, only in FULL mode)
SELECT content FROM memories
WHERE user_id = :uid AND memory_type = 'profile'
  AND category IN ('preference', 'workflow') AND is_active = 1
  AND MATCH(content) AGAINST(:query IN BOOLEAN MODE);
```

---

## L0/L1 Independent Sections

Split single `memory` section into two independently compressible sections:

```python
_SECTION_ORDER = [
    "identity", "self_model", "project_context",
    "memory_profile",    # L0 — independent budget & compression
    "memory_retrieval",  # L1 — independent budget & compression
    "working_memory", "history", "constraints",
]
```

Compression priority (first dropped = least important):
1. `memory_profile` L0-topic entries (L0-core never dropped)
2. `memory_retrieval` lowest-scored entries
3. `history` old turns
4. `working_memory`

This prevents the current failure mode: large L0 causes high-relevance L1 to be dropped when total `memory` section exceeds budget.

---

## Shared Embedding

Tier 1 runs three parallel tasks. All need query embedding — compute once:

```python
query_embedding = embed(query)  # 1 call

async with TaskGroup() as tg:
    tg.create_task(classify(query))
    tg.create_task(retrieve_and_compress(query, query_embedding))
    tg.create_task(prune_tools(query, query_embedding))
```

3 embedding calls → 1. Cost: $0.00003 → $0.00001/turn.

---

## Negative Feedback Signal

### Sources

| Signal | Trigger | Penalty |
|--------|---------|---------|
| User correction | `intent_correction` event ("不对/wrong") | ×0.3 |
| Firewall flag | Response confidence < threshold | ×0.5 |
| Same-session dedup | Memory already surfaced this session | ×0.7 |

### Implementation

Penalty applied in `MemoryRetriever._merge()` after base scoring:

```python
# In _merge(), after computing base final score:
if correction_ids and c.memory_id in correction_ids:
    final *= 0.3
if firewall_flagged_ids and c.memory_id in firewall_flagged_ids:
    final *= 0.5
if surfaced_ids and c.memory_id in surfaced_ids:
    final *= 0.7
```

### Correction Event

```python
{
    "event_type": "intent_correction",
    "memory_ids_used": ["mem-001", "mem-002"],
    "context_capture_id": "snap-xxx",
}
```

`memory_ids_used` sourced from context snapshot. Loaded at L1 retrieval time via:

```sql
SELECT DISTINCT JSON_EXTRACT(content, '$.memory_ids_used[*]')
FROM agent_events
WHERE user_id = :uid AND event_type = 'intent_correction'
  AND created_at > DATE_SUB(NOW(), INTERVAL 7 DAY);
```

---

## Prompt Cache Amplification

### Current

Provider caches stable prefix: `identity + self_model + constraints` (~750 tok).
Variable suffix changes every turn → no cache hit beyond prefix.

### With Memory Fingerprint

```
memory_fingerprint = sha256(sorted(L0_core_ids) + sorted(L1_top_memory_ids))
```

Same-intent turns with same memory → deeper cache hit:

```
Turn 1 (question):            750 (cached) + 200 (memory) + 1500 (rest) = 1700 billed
Turn 2 (question, same topic): 950 (cached: prefix+memory) + 1500 (rest) = 1500 billed
Turn 3 (preference):          750 (cached) + 50 (L0-core)               = 50 billed
Turn 4 (command):             750 (cached) + 0                           = 0 billed
```

---

## Error Handling（与 Router v3 对齐）

| Scenario | Memory behavior |
|----------|----------------|
| Tier 0 no match | → Tier 1 decides memory_mode |
| Tier 1 confidence < threshold | → `FULL` mode (safe fallback) |
| Tier 1 LLM call fails | → `FULL` mode (same as no routing) |
| User correction ("不对/wrong") | → reclassify as question + `FULL` mode + log `intent_correction` with `memory_ids_used` |
| Agent has `model_override` | → bypass routing, always `FULL` mode |
| First turn of session | → `FULL` mode (no history for classification) |

---

## Implementation Plan

### Phase 1: L0 Split + Independent Sections

No dependency on Router. Immediate value.

1. Add `category` to existing profile memories (`identity`/`constraint` vs `preference`/`workflow`)
2. Split `memory` → `memory_profile` + `memory_retrieval` in `_SECTION_ORDER`
3. Update `_compress()` priority
4. Update `_save_snapshot()` to track both sections

**Files**: `core/memory/tiered_loader.py`, `core/context/prompt_assembler.py`

### Phase 2: Route-Driven Loading

Depends on Router Tier 0/1 implementation.

1. Add `memory_mode` to `RouteDecision`
2. Wire `PromptAssembler.assemble()` to respect `context_plan["memory"]`
3. Skip `TieredMemoryLoader.build_section()` for `NONE` mode
4. Use Tier 1 compressed output for `COMPRESSED` mode

**Files**: `core/context/prompt_assembler.py`, `api/routers/chat.py`

### Phase 3: Negative Feedback + Shared Embedding

1. Log `memory_ids_used` in context snapshots
2. Load correction history in `MemoryRetriever.retrieve()`
3. Add penalty in `_merge()`
4. Refactor Tier 1 parallel tasks to share `query_embedding`

**Files**: `core/memory/retriever.py`, `core/events/event_logger.py`

### Phase 4: Cache Fingerprinting

1. Compute `memory_fingerprint` from sorted memory IDs
2. Include in prompt prefix for deeper provider cache hits
3. Monitor `cache_hit_ratio` improvement

**Files**: `core/context/prompt_assembler.py`

---

## Monitoring

| Metric | Target | Description |
|--------|--------|-------------|
| `memory_load_mode` | distribution | Count per mode (NONE/L0_CORE/COMPRESSED/FULL) |
| `memory_tokens_per_turn` | **< 200 avg** | Down from ~800 |
| `memory_retrieval_skipped_rate` | **> 40%** | % turns where L1 skipped |
| `memory_correction_penalty_applied` | count | Negative feedback usage |
| `memory_cache_fingerprint_hit_rate` | **> 60%** | Same-fingerprint cache hits |
| `memory_l0_core_tokens` | **< 60** | L0-core stays compact |

---

## Risks & Mitigations

| Risk | Mitigation |
|------|-----------|
| Misclassified intent skips needed memory | confidence < threshold → FULL (safe fallback) |
| L0 category assignment wrong | Conservative: only `identity`/`constraint` in L0-core |
| Compression loses critical L1 | Tier 1 uses cheapest LLM (semantic compress, not truncation) |
| Negative penalty too aggressive | Start ×0.7, tune via `intent_correction_rate` |
| Shared embedding adds coupling | Stateless — one task's failure doesn't affect others |
| First turn has no history for routing | First turn always FULL mode |
