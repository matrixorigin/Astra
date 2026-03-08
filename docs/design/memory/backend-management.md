# Memory Backend Management — Versioned, Per-User, Experimentable

> **Status**: Draft — pending review
> **Depends on**: [backend-coexistence.md](backend-coexistence.md), [graph-memory.md](graph-memory.md)
> **Related**: `core/sandbox/`, `core/git_for_data.py`, `core/evaluation/regression_gate.py`

---

## 1. Problem Statement

### What We Need

1. **Tune & Compare** — run different backends (or same backend with different params/versions) side-by-side, evaluate which is better on golden sessions
2. **Per-User Binding** — each user has exactly one active memory backend; different users can use different backends
3. **Seamless Migration** — upgrade backend version or switch backend type without losing memories
4. **Memory Injection & Correction** — admin/user can inject, correct, or purge specific memories; user can trigger learning/purification
5. **Memory Experiments** — "thought experiments" where you test memory changes in isolation before committing
6. **Git-for-Data Integration** — leverage MatrixOne's snapshot, branch, diff, merge for all of the above
7. **Many Backends** — architecture must support N backends, not just 2; each backend is fully independent

### What We Have

| Capability | Status | Location |
|---|---|---|
| Protocol-based interface (MemoryReader/Writer/Admin) | ✅ | `core/memory/interfaces.py` |
| Factory with backend selector | ✅ | `core/memory/factory.py` |
| Two backends (tabular, graph) | ⚠️ | graph depends on tabular (see §1.1) |
| Config with `memory_backend` field | ✅ | `core/memory/config.py` |
| Sandbox (zero-copy branch + PITR) | ✅ | `core/sandbox/sandbox.py` |
| Branch (diff, merge, snapshot) | ✅ | `core/sandbox/branch.py` |
| Git-for-Data (snapshot, time-travel, restore) | ✅ | `core/git_for_data.py` |
| Regression gate (golden sessions + sandbox replay) | ✅ | `core/evaluation/regression_gate.py` |
| Per-user backend binding | ❌ | Not implemented |
| Memory injection/correction API | ❌ | Not implemented |
| Memory experiment (sandbox memory) | ❌ | Not implemented |

### 1.1 Current Problem: Graph Is Not Independent

当前 `GraphMemoryService` 不是一个独立 backend，而是 tabular 的装饰器：

```
GraphMemoryService
├── store()          → tabular.store() + graph ingest
├── observe_turn()   → tabular.observe_turn() + graph ingest
├── retrieve()       → graph activation || tabular.retrieve() fallback
├── get_profile()    → tabular.get_profile()
├── run_governance() → tabular.run_governance() + graph consolidation
├── health_check()   → tabular.health_check()
└── candidates()     → graph candidates || tabular candidates fallback
```

这意味着：
- Graph 不能独立运行 — 它依赖 tabular 做存储、profile、governance
- 如果未来有 backend C，它也要依赖 tabular？还是依赖 graph？
- 无法真正 A/B 比较 — graph 的结果里混着 tabular 的 fallback

**根本原因**：graph 被设计为 tabular 的增强层（retrieval index），而不是独立的存储+检索系统。

### 1.2 Design Decision: Layered vs Independent

有两种架构方向：

**方向 A：独立 Backend（每个 backend 自包含）**

```
┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐
│tabular:v1│  │ graph:v1 │  │  kg:v1   │  │hybrid:v1 │
│          │  │          │  │          │  │          │
│ store    │  │ store    │  │ store    │  │ store    │
│ retrieve │  │ retrieve │  │ retrieve │  │ retrieve │
│ govern   │  │ govern   │  │ govern   │  │ govern   │
│ profile  │  │ profile  │  │ profile  │  │ profile  │
└──────────┘  └──────────┘  └──────────┘  └──────────┘
     各自独立，不互相依赖
```

- 优点：真正可比较、可替换、可独立演进
- 缺点：每个 backend 都要实现完整的 store/retrieve/govern/profile
- 迁移：需要数据导出/导入

**方向 B：Storage Layer + Retrieval Strategy（存储统一，检索可插拔）**

```
┌─────────────────────────────────────────────┐
│           Unified Storage Layer              │
│  mem_memories (canonical store of record)    │
│  store() / observe_turn() / governance()    │
└──────────────────────┬──────────────────────┘
                       │ feeds
        ┌──────────────┼──────────────┐
        ▼              ▼              ▼
┌──────────────┐ ┌──────────────┐ ┌──────────────┐
│ vector:v1    │ │activation:v1 │ │  hybrid:v1   │
│(flat cosine) │ │(graph+spread)│ │(vector+graph)│
│              │ │              │ │              │
│ retrieve()   │ │ retrieve()   │ │ retrieve()   │
│ (+ optional  │ │ (+ optional  │ │ (+ optional  │
│  index tables│ │  graph tables│ │  both)       │
└──────────────┘ └──────────────┘ └──────────────┘
     检索策略可插拔，存储层统一
```

- 优点：存储不重复、迁移免费（只切检索策略）、profile/governance 只实现一次
- 缺点：检索策略受限于统一存储的 schema
- 迁移：切换检索策略 = 零成本；检索策略的辅助表（graph nodes/edges）需要 backfill

**方向 C：混合 — Storage Layer + 可选 Index Layer + Retrieval Strategy**

```
┌─────────────────────────────────────────────┐
│           Canonical Storage                  │
│  mem_memories (source of truth)              │
│  store() / observe_turn()                   │
└──────────────────────┬──────────────────────┘
                       │ feeds (async or sync)
┌──────────────────────▼──────────────────────┐
│           Index Layer (optional per strategy)│
│  memory_graph_nodes/edges (for activation)  │
│  future: knowledge_triples (for KG)         │
│  future: episode_chains (for episodic)      │
└──────────────────────┬──────────────────────┘
                       │ used by
┌──────────────────────▼──────────────────────┐
│           Retrieval Strategy (pluggable)     │
│  vector:v1    — cosine on mem_memories      │
│  activation:v1 — graph spreading activation │
│  kg:v1        — knowledge graph traversal   │
│  hybrid:v1    — ensemble of multiple        │
└─────────────────────────────────────────────┘
                       │
┌──────────────────────▼──────────────────────┐
│           Shared Services                    │
│  profile / governance / reflection           │
│  (operate on canonical storage)              │
└─────────────────────────────────────────────┘
```

- 优点：存储统一（迁移免费）、检索可插拔、index 层按需构建、shared services 不重复
- 缺点：比方向 A 复杂一点
- 迁移：切换检索策略 = 零成本 + 可能需要 backfill index

**推荐方向 C**。原因：
1. 当前 graph 的本质就是 "canonical storage + graph index + activation retrieval"
2. 未来的 backend 大概率也是 "同样的 memories + 不同的 index + 不同的 retrieval"
3. Profile、governance、reflection 是通用的，不应该每个 backend 重新实现
4. 用户切换 retrieval strategy 不丢数据（canonical storage 不变）

---

## 2. Architecture Overview (Direction C)

```
┌─────────────────────────────────────────────────────────────┐
│                    Per-User Config                           │
│  user_id → { strategy: "activation:v1", params: {...} }     │
│  Resolution: per-user row → env var → "vector:v1"           │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│                  MemoryService (facade)                      │
│                                                             │
│  ┌────────────────────────────────────────────────────────┐ │
│  │              Canonical Storage                          │ │
│  │  mem_memories table (source of truth for ALL users)     │ │
│  │  store() / observe_turn() / get_profile()              │ │
│  │  governance / reflection / opinion evolution            │ │
│  └────────────────────────┬───────────────────────────────┘ │
│                           │                                 │
│  ┌────────────────────────▼───────────────────────────────┐ │
│  │         Retrieval Strategy (pluggable, per-user)       │ │
│  │                                                        │ │
│  │  ┌──────────┐ ┌──────────────┐ ┌────────────────────┐ │ │
│  │  │vector:v1 │ │activation:v1 │ │ activation:v2      │ │ │
│  │  │          │ │              │ │ (future: HNSW +    │ │ │
│  │  │ cosine   │ │ graph spread │ │  different params) │ │ │
│  │  │ on mem_  │ │ on graph_   │ │                    │ │ │
│  │  │ memories │ │ nodes/edges │ │                    │ │ │
│  │  └──────────┘ └──────────────┘ └────────────────────┘ │ │
│  │                                                        │ │
│  │  Each strategy:                                        │ │
│  │  - retrieve(user_id, query, embedding) → list[Memory]  │ │
│  │  - may own auxiliary index tables                       │ │
│  │  - may need backfill when first assigned to a user     │ │
│  └────────────────────────────────────────────────────────┘ │
│                                                             │
│  ┌────────────────────────────────────────────────────────┐ │
│  │         Index Manager (per-strategy)                    │ │
│  │                                                        │ │
│  │  On store()/observe_turn():                            │ │
│  │    → canonical storage writes mem_memories             │ │
│  │    → index manager updates strategy-specific tables    │ │
│  │      (e.g., graph_nodes/edges for activation:v1)      │ │
│  │                                                        │ │
│  │  On strategy switch:                                   │ │
│  │    → backfill new strategy's index from mem_memories   │ │
│  └────────────────────────────────────────────────────────┘ │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│              Memory Operations Layer                         │
│  Inject / Correct / Purge / Relearn / Experiment            │
│  (operates on canonical storage, triggers index update)     │
└──────────────────────────┬──────────────────────────────────┘
                           │
┌──────────────────────────▼──────────────────────────────────┐
│              Git-for-Data Integration                        │
│  Snapshot / Branch / Diff / Merge / Restore                 │
└─────────────────────────────────────────────────────────────┘
```

### 2.1 Key Separation

| Layer | Responsibility | Multiplicity |
|---|---|---|
| Canonical Storage | store, observe_turn, profile, governance, reflection | **One** (shared by all users, all strategies) |
| Retrieval Strategy | retrieve() only | **Many** (pluggable, per-user) |
| Index Manager | Maintain strategy-specific auxiliary tables | **Per-strategy** (graph nodes/edges, KG triples, etc.) |
| Memory Operations | inject, correct, purge, relearn, experiment | **One** (operates on canonical storage) |

### 2.2 What Changes from Current Design

| Current | New |
|---|---|
| `TabularMemoryService` = storage + retrieval | Split: storage stays, retrieval becomes `vector:v1` strategy |
| `GraphMemoryService` = tabular wrapper + graph retrieval | Split: graph ingest becomes index manager, activation becomes `activation:v1` strategy |
| `GraphMemoryService.store()` calls `tabular.store()` | Canonical storage calls `store()` once, then notifies active index manager |
| `GraphMemoryService.retrieve()` falls back to tabular | Each strategy is self-contained; no fallback chain |
| Backend = entire service class | Backend = retrieval strategy + optional index |

### 2.3 Refactoring GraphMemoryService

Current `GraphMemoryService` decomposes into:

```
GraphMemoryService (current monolith)
    │
    ├── store/observe_turn/profile/governance/health
    │   → moves to Canonical Storage (shared)
    │
    ├── graph ingest (GraphBuilder)
    │   → becomes ActivationIndexManager
    │   → called by canonical storage after store/observe_turn
    │
    ├── activation retrieval (ActivationRetriever)
    │   → becomes activation:v1 RetrievalStrategy
    │
    ├── graph consolidation (GraphConsolidator)
    │   → stays with ActivationIndexManager (index maintenance)
    │
    └── opinion evolution
        → stays with ActivationIndexManager (index maintenance)
```

---

## 3. Retrieval Strategy

### 3.1 Protocol

```python
class RetrievalStrategy(Protocol):
    """Pluggable retrieval strategy. Only responsible for retrieve()."""

    @property
    def strategy_key(self) -> str:
        """Unique key: 'vector:v1', 'activation:v1', etc."""
        ...

    def retrieve(
        self, user_id: str, query: str,
        query_embedding: list[float] | None = None,
        *, top_k: int = 10,
        task_type: str | None = None,
    ) -> list[Memory]:
        """Retrieve memories ranked by this strategy's algorithm."""
        ...
```

### 3.2 Index Manager Protocol

```python
class IndexManager(Protocol):
    """Maintains auxiliary index tables for a retrieval strategy."""

    def on_memories_stored(
        self, user_id: str, memories: list[Memory],
        *, session_id: str | None = None,
    ) -> None:
        """Called after canonical storage writes. Update index."""
        ...

    def on_governance(self, user_id: str) -> None:
        """Called during governance. Maintain index health."""
        ...

    def backfill(self, user_id: str) -> BackfillResult:
        """Backfill index from canonical storage (for new strategy assignment)."""
        ...
```

### 3.3 Built-in Strategies

| Strategy Key | Retrieval | Index Tables | Index Manager |
|---|---|---|---|
| `vector:v1` | Cosine similarity on `mem_memories.embedding` | None (uses canonical table directly) | None needed |
| `activation:v1` | Spreading activation on graph | `memory_graph_nodes`, `memory_graph_edges` | `ActivationIndexManager` (graph builder + consolidator + opinion) |

### 3.4 Strategy Registry

```python
@dataclass(frozen=True)
class StrategyDescriptor:
    """Identifies a retrieval strategy + version + params."""
    strategy_type: str     # "vector" | "activation" | "kg" | ...
    version: str           # "v1", "v2"
    params: dict[str, Any] = field(default_factory=dict)

    @property
    def key(self) -> str:
        return f"{self.strategy_type}:{self.version}"

class StrategyRegistry:
    """Registry of available retrieval strategies."""

    def register(
        self, key: str,
        strategy_cls: type[RetrievalStrategy],
        index_manager_cls: type[IndexManager] | None = None,
    ) -> None: ...

    def create_strategy(self, descriptor: StrategyDescriptor, **deps) -> RetrievalStrategy: ...
    def create_index_manager(self, descriptor: StrategyDescriptor, **deps) -> IndexManager | None: ...
    def list_available(self) -> list[str]: ...
```

### 3.5 Fallback Behavior

每个 strategy 是自包含的，**没有 fallback chain**。如果 `activation:v1` 的 graph 节点不够（< MIN_GRAPH_NODES），它自己决定怎么处理：

```python
class ActivationStrategy:
    def retrieve(self, user_id, query, query_embedding, **kw):
        if not self._store.has_min_nodes(user_id, MIN_GRAPH_NODES):
            # Strategy-internal decision: use vector fallback on canonical table
            return self._vector_fallback(user_id, query_embedding, **kw)
        return self._activation_retrieve(user_id, query, query_embedding, **kw)
```

这和"graph fallback 到 tabular backend"不同 — 这是 activation strategy 内部的实现细节，不是 backend 间的依赖。

---

## 4. Per-User Strategy Binding

### 4.1 Schema

```sql
CREATE TABLE mem_user_memory_config (
    user_id           VARCHAR(64) PRIMARY KEY,
    strategy_key      VARCHAR(32) NOT NULL DEFAULT 'vector:v1',
    params_json       JSON,           -- strategy-specific param overrides
    migrated_from     VARCHAR(32),    -- previous strategy (for rollback)
    migration_snapshot VARCHAR(64),   -- snapshot taken before migration
    index_status      VARCHAR(20) NOT NULL DEFAULT 'ready',
        -- ready | backfilling | failed
    created_at        DATETIME(6) DEFAULT NOW(),
    updated_at        DATETIME(6) DEFAULT NOW()
);
```

### 4.2 Resolution Order

```
1. mem_user_memory_config.strategy_key  (per-user, if row exists)
2. MEM_RETRIEVAL_STRATEGY env var       (system-wide default)
3. "vector:v1"                          (hardcoded fallback)
```

### 4.3 Factory Change

```python
def create_memory_service(
    db_factory: DbFactory,
    *,
    user_id: str | None = None,
    strategy: str | None = None,   # explicit override (testing/admin)
    **kwargs,
) -> MemoryService:
    """Create memory service with per-user retrieval strategy."""
    if strategy is None:
        strategy = _resolve_strategy(db_factory, user_id)
    descriptor = StrategyDescriptor.parse(strategy)
    retrieval = _registry.create_strategy(descriptor, db_factory=db_factory, **kwargs)
    index_mgr = _registry.create_index_manager(descriptor, db_factory=db_factory, **kwargs)
    return MemoryService(
        storage=CanonicalStorage(db_factory, **kwargs),
        retrieval=retrieval,
        index_manager=index_mgr,
    )
```

### 4.4 Strategy Switch Flow

```
User requests switch from vector:v1 → activation:v1:
1. Check: ActivationIndexManager.backfill_needed(user_id)?
2. If yes: set index_status = 'backfilling'
3. Submit async backfill job (same framework as GovernanceScheduler)
4. User continues using old strategy during backfill
5. User-facing status: "正在为您的记忆构建加速索引（约 2 分钟）"
6. On success: update strategy_key, set index_status = 'ready'
7. On failure: set index_status = 'failed', keep old strategy_key, notify user
```

```python
# Strategy switch implementation
def switch_strategy(self, user_id: str, new_strategy: str) -> SwitchResult:
    descriptor = StrategyDescriptor.parse(new_strategy)
    index_mgr = _registry.create_index_manager(descriptor, ...)

    if index_mgr and index_mgr.backfill_needed(user_id):
        self._update_index_status(user_id, "backfilling")
        # Async backfill — same job framework as GovernanceScheduler
        background_task(
            self._backfill_and_switch,
            user_id=user_id,
            new_strategy=new_strategy,
        )
        return SwitchResult(status="backfilling", estimated_seconds=120)

    # No backfill needed (e.g., switching to vector:v1)
    self._update_strategy_key(user_id, new_strategy)
    return SwitchResult(status="ready")
```

Backfill is **always async**. Estimated time for 10K memories: ~2 minutes (graph builder processes ~80 memories/sec with embedding + edge creation). Progress is visible via `index_status` field, queryable by CLI/API.

During backfill, `retrieve()` continues using the old strategy. Switch is atomic after backfill completes.

---

## 5. Strategy Migration

### 5.1 Why Migration Is (Mostly) Free

Canonical storage (`mem_memories`) is shared by all strategies. Switching strategy only affects retrieval, not data.

| Migration Type | Cost | What Happens |
|---|---|---|
| vector:v1 → activation:v1 | Backfill graph index | Build graph_nodes/edges from mem_memories |
| activation:v1 → vector:v1 | Zero | vector:v1 reads mem_memories directly |
| activation:v1 → activation:v2 | Depends | If v2 changes graph schema, rebuild index |
| Any → new strategy | Backfill | New strategy's index manager builds its tables |

### 5.2 Index Backfill Protocol

```python
class IndexManager(Protocol):
    def backfill(self, user_id: str, after_memory_id: str | None = None) -> BackfillResult:
        """Build index from canonical storage for this user.

        Must be idempotent — safe to re-run on failure.
        Incremental: only processes memory_id > after_memory_id (skip already-indexed).
        """
        ...

    def drop_index(self, user_id: str) -> None:
        """Remove this user's data from index tables."""
        ...
```

### 5.3 Async Backfill Execution

Backfill runs as a **fully async background task** (via GovernanceScheduler / job queue). The user is never blocked.

```
1. Snapshot: CREATE SNAPSHOT mem_migrate_{user_id}_{timestamp}
2. Set index_status = 'building' on mem_user_memory_config
3. Enqueue: backfill job (async, background)
   - Incremental: WHERE memory_id > last_indexed_id
   - Emits memory_index_progress events (% complete + ETA)
4. On completion: index_status = 'ready', switch strategy_key
5. On failure: index_status = 'failed', user stays on old strategy
6. Cleanup: old_index_manager.drop_index(user_id) (async, after 30-day grace)
```

**Dynamic ETA estimation:**

```python
# Inside backfill job, updated every 100 memories processed
avg_speed = processed_count / elapsed_seconds          # memories/sec
estimated_remaining = (total - processed_count) / avg_speed * 1.2  # +20% buffer
```

Initial ETA (before any history) uses a conservative default of 80 memories/sec (measured baseline on MatrixOne with embedding + graph node creation). After the first batch completes, ETA switches to actual observed speed.

**Queue throttling:**

- Global concurrency limit: **5 simultaneous backfill jobs** (configurable via `BACKFILL_MAX_CONCURRENT`)
- Jobs beyond the limit are queued with FIFO ordering
- CLI shows queue position: `Strategy: activation:v1 (queued — position 2 of 3)`
- Priority override: admin can bump a job with `mo-agent memory backfill prioritize <user_id>`

**Progress visibility:**

```bash
mo-agent memory status
# Strategy: activation:v1 (building index... 67% — ~45s remaining)
# or: Strategy: activation:v1 (queued — position 2 of 3)
```

Progress events (`memory_index_progress`) are emitted to `conversation_events`, consumable by CLI and Web UI.

整个流程的原子性由 MatrixOne transaction + snapshot 保证。Snapshot 是零成本创建。回滚 100% 安全 — `RESTORE FROM SNAPSHOT` 恢复到精确的 snapshot 时间点。

### 5.4 Version Upgrade (Same Strategy Type)

```
activation:v1 → activation:v2 (schema change):
1. Snapshot
2. v2 IndexManager rebuilds index tables (may ALTER TABLE or create new tables)
3. Verify
4. Switch strategy_key
5. Drop v1 index data
```

---

## 6. Memory Injection, Correction & Purge

### 6.1 Operations

| Operation | Who | What | Event Logged |
|---|---|---|---|
| **Inject** | Admin | Insert memory with T1_VERIFIED trust tier | `memory_injected` |
| **Correct** | User/Admin | Supersede existing memory with corrected version | `memory_corrected` |
| **Purge** | User | Deactivate memories matching criteria + run governance | `memory_purged` |
| **Re-learn** | User | Trigger re-consolidation with tuned params | `memory_relearned` |

### 6.2 Interface Extension

```python
class MemoryEditor(Protocol):
    """Memory injection, correction, and purge operations."""

    def inject(
        self, user_id: str, content: str, *,
        memory_type: MemoryType,
        trust_tier: TrustTier = TrustTier.T1_VERIFIED,
        source: str = "admin_inject",
    ) -> Memory:
        """Inject a memory with high trust. Logged as event."""
        ...

    def correct(
        self, user_id: str, memory_id: str, new_content: str, *,
        reason: str = "",
    ) -> Memory:
        """Supersede a memory with corrected version. Old memory deactivated."""
        ...

    def purge(
        self, user_id: str, *,
        memory_ids: list[str] | None = None,
        memory_types: list[MemoryType] | None = None,
        before: datetime | None = None,
        reason: str = "",
    ) -> PurgeResult:
        """Deactivate memories matching criteria. Snapshot taken first."""
        ...

    def relearn(
        self, user_id: str, *,
        config_overrides: dict[str, Any] | None = None,
    ) -> RelearnResult:
        """Re-run consolidation + reflection with (optionally tuned) params."""
        ...
```

### 6.3 Safety: Snapshot Before Destructive Ops

```python
def purge(self, user_id, *, reason, **criteria):
    # 1. Snapshot before purge
    snapshot = f"pre_purge_{user_id}_{now_compact()}"
    self._git.create_snapshot(snapshot)

    # 2. Execute purge
    result = self._do_purge(user_id, **criteria)

    # 3. Log event with snapshot reference
    self._logger.create_event(
        event_type="memory_purged",
        user_id=user_id,
        content=json.dumps({"reason": reason, "snapshot": snapshot, **result}),
    )
    return result
```

用户后悔了？`RESTORE FROM SNAPSHOT pre_purge_...` 即可恢复。

---

## 7. Memory Experiments (Thought Experiments)

### 7.1 Concept

"如果我改变这些记忆，agent 的回答会怎样变？" — 在不影响生产数据的情况下测试记忆变更。

### 7.2 Experiment Lifecycle

```
┌─────────────────────────────────────────────────────┐
│                 Production Memory                    │
│  mem_memories + memory_graph_nodes (user's real data)│
└──────────────────────┬──────────────────────────────┘
                       │ data branch create (zero-copy)
                       ▼
┌─────────────────────────────────────────────────────┐
│              Experiment Branch                        │
│  exp_{id}.mem_memories + exp_{id}.memory_graph_nodes │
│                                                      │
│  Mutations:                                          │
│  - inject / correct / purge (on branch only)         │
│  - re-run consolidation with different params        │
│  - manually edit memories                            │
└──────────────────────┬──────────────────────────────┘
                       │ evaluate
                       ▼
┌─────────────────────────────────────────────────────┐
│              Evaluation                              │
│  - Replay golden sessions against experiment branch  │
│  - Diff: data branch diff exp vs production          │
│  - Metrics: retrieval quality, profile accuracy      │
└──────────────────────┬──────────────────────────────┘
                       │ decision
                 ┌─────┴─────┐
                 ▼           ▼
              Commit       Discard
         (branch merge)  (drop branch)
```

### 7.3 Schema

```sql
CREATE TABLE mem_experiments (
    experiment_id   VARCHAR(36) PRIMARY KEY,
    user_id         VARCHAR(64) NOT NULL,
    name            VARCHAR(100) NOT NULL,
    description     TEXT,
    status          VARCHAR(20) NOT NULL DEFAULT 'active',
        -- active | evaluating | committed | discarded
    branch_db       VARCHAR(64) NOT NULL,  -- sandbox database name
    base_snapshot   VARCHAR(64),           -- snapshot at branch point
    config_json     JSON,                  -- param overrides for this experiment
    metrics_json    JSON,                  -- evaluation results
    created_at      DATETIME(6) DEFAULT NOW(),
    committed_at    DATETIME(6),
    created_by      VARCHAR(64) NOT NULL
);
```

### 7.4 API

```python
class MemoryExperiment:
    """Isolated memory experiment using Git-for-Data branching."""

    def create(
        self, user_id: str, name: str, *,
        description: str = "",
        config_overrides: dict | None = None,
    ) -> ExperimentInfo:
        """Create experiment: snapshot + branch memory tables.

        Automatically:
        1. CREATE SNAPSHOT base_{experiment_id} (rollback safety net)
        2. data branch create for mem_memories + index tables
        """
        ...

    def get_service(self, experiment_id: str) -> MemoryService:
        """Get a MemoryService that reads/writes to the experiment branch."""
        ...

    def diff(self, experiment_id: str) -> ExperimentDiff:
        """Three-way diff: experiment vs production (auto LCA).

        Returns structured report:
        - memories_added: list of new memory_ids
        - memories_modified: list of (memory_id, field, old, new)
        - memories_removed: list of deactivated memory_ids
        - scenes_added: list of new scene node_ids (graph strategy)
        - index_changes: summary of index table diffs
        """
        ...

    def evaluate(
        self, experiment_id: str, *,
        golden_session_ids: list[str] | None = None,
    ) -> EvalResult:
        """Replay golden sessions against experiment branch.

        After evaluation, auto-generates diff report and attaches
        to experiment's metrics_json for audit trail.

        Returns EvalResult with standard metrics (see §8.4).
        """
        ...

    def commit(self, experiment_id: str) -> None:
        """Merge experiment branch into production.

        Optimistic locking: compares base_snapshot timestamp against
        current production state. If production changed since branch
        point, commit fails with ConflictError — user must re-evaluate.
        """
        ...

    def discard(self, experiment_id: str) -> None:
        """Drop experiment branch. Base snapshot retained for audit."""
        ...

    def extend_ttl(self, experiment_id: str, days: int = 7) -> None:
        """Extend experiment TTL. CLI: mo-agent experiment extend <id>."""
        ...
```

### 7.5 Git-for-Data Mapping

| Experiment Op | MatrixOne Primitive |
|---|---|
| Create experiment | `CREATE SNAPSHOT base_{id}` + `data branch create table` |
| Mutate experiment | Normal SQL on branch tables |
| Diff | `data branch diff exp.table against prod.table` |
| Commit | `data branch merge exp.table into prod.table` |
| Discard | `DROP DATABASE exp_db` (base snapshot retained for audit) |
| Rollback commit | `RESTORE FROM SNAPSHOT base_{id}` |

### 7.6 Experiment TTL & Cleanup

- Default TTL: **7 days** from creation
- User can extend: `mo-agent experiment extend <id>` (adds 7 days, max 30 days total)
- Auto-cleanup job (runs daily with GovernanceScheduler):
  1. Find experiments where `status = 'active' AND created_at + TTL < NOW()`
  2. Set `status = 'expired'`
  3. `DROP DATABASE branch_db` to reclaim storage
  4. Retain `mem_experiments` row + `base_snapshot` for audit trail
- Orphaned branch detection: if `branch_db` exists but no `mem_experiments` row references it, drop after 24h

### 7.7 Concurrency Limits

- **Per-user cap**: max **3 active experiments** (configurable via `mem_user_memory_config.max_experiments`, default 3)
- Creating a 4th experiment returns `ExperimentLimitError` with list of active experiments
- `committed` and `discarded` experiments don't count toward the limit
- Admin override: `--force` flag bypasses the cap

### 7.8 Commit-Time Diff Summary

Before commit, the system auto-generates a structured change summary:

```
mo-agent memory experiment commit exp_123

  Change Summary (auto-generated):
  ├─ 3 memories injected (2 semantic, 1 procedural)
  ├─ 1 memory corrected (mem_456: preference updated)
  ├─ 5 memories purged (old Java preferences)
  ├─ Graph: +12 nodes, +28 edges, -5 nodes
  └─ Eval: precision@10 improved 0.72 → 0.81

  LLM Summary: "This commit will make the agent prioritize Docker base
  image checks during CI debugging. Expected multi-hop accuracy +18%."

  Proceed? [y/N]
```

The structured diff is generated programmatically. The natural language summary is a single LLM call on the diff output, giving the user a plain-English explanation of behavioral impact. Both are persisted in `mem_experiments.metrics_json` for audit.

---

## 8. Strategy Tuning & A/B Comparison

### 8.1 Tuning Workflow

```
1. Create experiment with params_override for current strategy
   e.g., activation:v1 + {"spreading_factor": 0.9, "num_iterations": 4}

2. Experiment branch gets same canonical data
   but index rebuilt with new params

3. Evaluate: replay golden sessions against experiment
   → compare retrieval quality metrics

4. If better: update user's params_json in mem_user_memory_config
   If worse: discard experiment
```

### 8.2 A/B Comparison (Two Strategies)

```
1. User currently on vector:v1
2. Create experiment with strategy_key="activation:v1"
3. Backfill activation index on experiment branch
4. Replay same golden sessions on both strategies
5. Compare metrics side-by-side
6. If activation wins: switch user to activation:v1
```

### 8.3 Integration with RegressionGate

现有的 `RegressionGate` 已经做了：
1. 加载 golden sessions
2. 创建 sandbox
3. 在 sandbox 中 replay
4. 比较 metrics
5. Pass/fail 决策

Memory experiment 的 evaluate 直接复用这个流程。

### 8.4 Standard Evaluation Metrics

Every experiment evaluation collects these metrics:

| Metric | Description | Source |
|---|---|---|
| `retrieval_precision_at_k` | % of top-K retrieved memories that are relevant | Golden session ground truth |
| `retrieval_recall_at_k` | % of relevant memories that appear in top-K | Golden session ground truth |
| `multi_hop_hit_rate` | % of queries where activation reached correct memory via edges (graph-specific) | Graph traversal trace |
| `avg_tokens_in_context` | Average token count of retrieved memories per query | Token counter |
| `response_quality_score` | LLM response quality scored against golden session expected output | Auto-scorer / golden comparison |
| `profile_accuracy` | Cosine similarity between generated profile and golden profile | Embedding comparison |
| `p50_retrieve_latency_ms` | Median retrieval latency | Timer |
| `p99_retrieve_latency_ms` | 99th percentile retrieval latency | Timer |

### 8.5 Strategy Params Validation

Each strategy defines a Pydantic schema for its tunable parameters:

```python
class VectorV1Params(BaseModel):
    """Params for vector:v1 retrieval strategy."""
    semantic_weight: float = Field(0.4, ge=0.0, le=1.0)
    temporal_weight: float = Field(0.3, ge=0.0, le=1.0)
    confidence_weight: float = Field(0.2, ge=0.0, le=1.0)
    importance_weight: float = Field(0.1, ge=0.0, le=1.0)

class ActivationV1Params(BaseModel):
    """Params for activation:v1 retrieval strategy."""
    spreading_factor: float = Field(0.8, ge=0.0, le=1.0)
    num_iterations: int = Field(3, ge=1, le=10)
    inhibition_beta: float = Field(0.15, ge=0.0, le=1.0)
    sigmoid_theta: float = Field(0.1, ge=0.0, le=1.0)
    min_graph_nodes: int = Field(50, ge=1)

# Registry maps strategy key → params schema
STRATEGY_PARAMS_SCHEMA: dict[str, type[BaseModel]] = {
    "vector:v1": VectorV1Params,
    "activation:v1": ActivationV1Params,
}
```

When `params_json` is written to `mem_user_memory_config` or `mem_experiments`, it is validated against the strategy's schema. Invalid params are rejected at write time.

---

## 9. Memory Programming Layer

> Analogy: Meta-programming lets code manipulate code. Memory Programming lets users (and agents) manipulate memories with the same declarative, version-controlled, sandboxed guarantees.

### 9.1 What It Is

A thin orchestration layer that composes existing primitives:

| Primitive | Source | Role in Memory Programming |
|---|---|---|
| `MemoryEditor` | §6 | Execute actions: inject / correct / purge / relearn |
| `MemoryExperiment` | §7 | Sandbox: branch → execute → diff → commit/discard |
| `git4data` | core | Snapshot, diff, rollback |
| `RetrievalStrategy` | §3 | Tune params via script |
| LLM | core/llm | Parse natural language → structured script |

**No new execution engine.** The programmer module is a script parser + action dispatcher. All side effects go through existing safe paths.

### 9.2 Script Format

```yaml
# my-memory-program.yml
version: 1
actions:
  - inject:
      content: "User prefers Python for data work, Go for system tools"
      type: semantic
      trust: T2
  - correct:
      memory_id: mem_123
      new_content: "User now prefers concise error messages"
  - purge:
      filter: { type: preference, content_contains: "Java" }
  - trigger_reflection:
      topic: "CI failure patterns"
  - tune:
      strategy: activation:v1
      params: { spreading_factor: 0.9 }
```

Natural language input is also accepted — LLM converts to this format before execution.

### 9.3 Execution Model

```
User input (natural language or YAML)
    │
    ▼
LLM parse (if natural language) → structured script
    │
    ▼
Validate (Pydantic schema per action type)
    │
    ▼
Create experiment branch (automatic)
    │
    ▼
Execute actions via MemoryEditor
    │
    ▼
Return diff + metrics
    │
    ▼
User: commit / discard / modify
```

Every execution is sandboxed by default. `--no-sandbox` available for admin batch ops with `--force` flag.

### 9.4 CLI Interface

```bash
# Natural language (LLM generates script)
mo-agent memory program "User now prefers Python for data science, Go only for system tools"

# YAML script
mo-agent memory program run my-memory-program.yml

# Batch (admin)
mo-agent memory program batch --script company-standards.yml --users all --dry-run

# LLM-assisted debug
mo-agent memory program debug "Why does agent still think I prefer Go?"

# Thought experiment
mo-agent memory program experiment "test-clean-slate" --script purge-old-prefs.yml

# Review pending, commit, discard
mo-agent memory program review
mo-agent memory program commit
mo-agent memory program discard
```

### 9.5 User Tiers & Permission Model

| Tier | Interface | YAML scripts? | Batch? |
|---|---|---|---|
| Casual | Natural language chat | ❌ (LLM generates internally) | ❌ |
| Developer | YAML scripts | ✅ (own memories only) | ❌ |
| Admin | Full access | ✅ | ✅ (`--users`) |

Casual users can only use natural language one-liners. YAML script execution requires `developer` role or above.

### 9.6 Safety Guarantees

- **Default sandbox**: All scripts execute in experiment branch
- **Auto-snapshot**: Pre-execution snapshot, always rollback-able
- **Dry-run**: `--dry-run` shows diff without applying
- **Permission scoping**: Users operate own memories only; admin required for `--users`
- **Audit trail**: Every execution logged to `mem_edit_log` + `conversation_events`

### 9.7 Automatic Safety Review

Every script goes through a **mandatory LLM safety review** before execution (default on, `--skip-review` for admin only):

```
Script (parsed) → LLM Safety Reviewer → risk_score (0-100) + warnings
    │
    ├─ risk < 30:  auto-approve, execute in sandbox
    ├─ risk 30-70: show warnings, require user confirmation
    └─ risk > 70:  block execution, require --force + admin role
```

**Dangerous operations** (always require `--require-approval`):
- `purge` affecting > 50 memories
- `trust` tier elevation (T3→T2, T2→T1)
- `tune` with extreme params (outside 2σ of defaults)
- `batch` targeting `--users all`

The reviewer is a separate LLM call with a "security auditor" system prompt — it does not share context with the script-generating LLM to prevent prompt injection bypass.

### 9.8 LLM Prompt Templates

**Natural language → YAML conversion prompt:**

```
You are a Memory Script Generator. Convert the user's natural language instruction
into a structured YAML memory program.

Rules:
- Output ONLY valid YAML matching the MemoryProgram schema (version, actions[])
- Each action must be one of: inject, correct, purge, trigger_reflection, tune
- For inject: infer memory type (semantic/procedural/episodic) from content
- Default trust tier: T2 (user-stated). Use T1 only if user says "verified" or "certain"
- Never invent memory_ids — use purge with filter instead of correct when no ID given
- If the instruction is ambiguous, generate the CONSERVATIVE interpretation

User instruction: {user_input}
Current user_id: {user_id}
Existing memory types in use: {active_types}

Output the YAML script and nothing else.
```

**Safety review prompt (independent LLM call):**

```
You are a Memory Safety Auditor. Review the following memory program script
for risks BEFORE it executes.

Evaluate these risk factors:
1. Scale: How many memories affected? (purge >50 = high risk)
2. Trust manipulation: Any trust tier elevation? (T3→T1 = high risk)
3. Content safety: Any injection of harmful/misleading content?
4. Irreversibility: Would this be hard to undo even with snapshot rollback?
5. Scope creep: Does the script do more than the user likely intended?

Script:
{yaml_script}

User's original instruction (if available):
{original_instruction}

Respond with JSON:
{
  "risk_score": 0-100,
  "risk_level": "low" | "medium" | "high",
  "warnings": ["specific concern 1", ...],
  "suggestions": ["improvement 1", ...],
  "approve": true | false
}
```

These prompts are stored in `core/memory/prompts.py` alongside existing memory prompts, versioned with the codebase.

### 9.9 Implementation

Single module: `core/memory/programmer.py`

```python
class MemoryProgrammer:
    """Thin orchestrator over MemoryEditor + MemoryExperiment."""

    def __init__(self, editor: MemoryEditor, experiments: MemoryExperimentManager):
        self.editor = editor
        self.experiments = experiments

    def execute(self, user_id: str, script: str | dict, sandbox: bool = True) -> ProgramResult:
        """Parse script → create experiment → execute actions → return diff."""
        ...
```

**Estimated effort**: 5-7 days (after Phase 0-2 complete). Zero new infrastructure — pure composition of existing primitives.

---

## 10. Data Model Summary

### New Tables

```sql
-- Per-user strategy binding
CREATE TABLE mem_user_memory_config (
    user_id           VARCHAR(64) PRIMARY KEY,
    strategy_key      VARCHAR(32) NOT NULL DEFAULT 'vector:v1',
    params_json       JSON,
    migrated_from     VARCHAR(32),
    migration_snapshot VARCHAR(64),
    index_status      VARCHAR(20) NOT NULL DEFAULT 'ready',
    created_at        DATETIME(6) DEFAULT NOW(),
    updated_at        DATETIME(6) DEFAULT NOW()
);

-- Memory experiments
CREATE TABLE mem_experiments (
    experiment_id   VARCHAR(36) PRIMARY KEY,
    user_id         VARCHAR(64) NOT NULL,
    name            VARCHAR(100) NOT NULL,
    description     TEXT,
    status          VARCHAR(20) NOT NULL DEFAULT 'active',
    branch_db       VARCHAR(64) NOT NULL,
    base_snapshot   VARCHAR(64),
    strategy_key    VARCHAR(32),
    params_json     JSON,
    metrics_json    JSON,
    created_at      DATETIME(6) DEFAULT NOW(),
    committed_at    DATETIME(6),
    created_by      VARCHAR(64) NOT NULL,
    INDEX idx_exp_user (user_id),
    INDEX idx_exp_status (status)
);

-- Memory edit audit log (injection, correction, purge)
CREATE TABLE mem_edit_log (
    edit_id         VARCHAR(36) PRIMARY KEY,
    user_id         VARCHAR(64) NOT NULL,
    operation       VARCHAR(20) NOT NULL,  -- inject | correct | purge | relearn
    target_ids      JSON,
    content         TEXT,
    reason          TEXT,
    snapshot_before VARCHAR(64),
    experiment_id   VARCHAR(36),
    created_at      DATETIME(6) DEFAULT NOW(),
    created_by      VARCHAR(64) NOT NULL,
    INDEX idx_edit_user (user_id),
    INDEX idx_edit_experiment (experiment_id)
);
```

### Existing Tables (Unchanged)

| Table | Role |
|---|---|
| `mem_memories` | Canonical storage (source of truth, shared by all strategies) |
| `memory_graph_nodes` | Index table for activation:v1 strategy |
| `memory_graph_edges` | Index table for activation:v1 strategy |
| `auth_users` | User identity |

---

## 11. Implementation Plan

### Phase 0: Full Decouple — Eliminate GraphMemoryService (Prerequisite)

> **Decision**: Full refactor, not minimal adapter. Rationale: Direction C's core premise is "no wrapper backends". Keeping GraphMemoryService as adapter accumulates technical debt that blocks all subsequent phases. Estimated: 2-3 days, all existing tests must pass before merge.

```
1. Extract CanonicalStorage from TabularMemoryService
   - store(), observe_turn(), get_profile(), run_governance(), health_check()
   - These operate on mem_memories and are strategy-agnostic

2. Extract VectorRetrievalStrategy from TabularMemoryService
   - retrieve() using cosine similarity on mem_memories.embedding
   - No index tables needed

3. Extract ActivationRetrievalStrategy from GraphMemoryService
   - retrieve() using spreading activation on graph_nodes/edges
   - Internal vector fallback when graph too small (strategy-internal, not cross-backend)

4. Extract ActivationIndexManager from GraphMemoryService
   - GraphBuilder.ingest() → on_memories_stored()
   - GraphConsolidator → on_governance()
   - Opinion evolution → on_memories_stored()
   - backfill() → build graph from existing mem_memories

5. New MemoryService facade: CanonicalStorage + RetrievalStrategy + IndexManager
6. DELETE GraphMemoryService and TabularMemoryService
7. All existing 122+ tests must pass (same behavior, different structure)
```

**Deliverable**: Clean separation. GraphMemoryService eliminated. Two strategies registered. Zero behavior change.

### Phase 1: Per-User Strategy Binding

```
1. Create mem_user_memory_config table
2. StrategyDescriptor + StrategyRegistry
3. Update create_memory_service to resolve per-user strategy
4. Strategy switch with backfill flow
5. Tests: factory resolves correctly, backfill works
```

**Deliverable**: Different users can use different retrieval strategies.

### Phase 2: Memory Editor (Inject/Correct/Purge)

```
1. MemoryEditor protocol + implementation on CanonicalStorage
2. mem_edit_log table for audit trail
3. Snapshot-before-destructive-ops safety net
4. Relearn: re-run consolidation + index rebuild with param overrides
5. Tests: inject/correct/purge with DB verification + snapshot rollback
```

**Deliverable**: Admin can inject memories, users can correct/purge/relearn.

### Phase 3: Memory Experiments

```
1. mem_experiments table
2. MemoryExperiment class using Sandbox + Branch
3. Experiment lifecycle: create → mutate → diff → evaluate → commit/discard
4. Integration with RegressionGate for evaluation
5. Tests: full experiment lifecycle E2E
```

**Deliverable**: Isolated memory experiments with Git-for-Data branching.

### Phase 4: Tuning & A/B

```
1. Param override propagation through experiment → strategy
2. A/B comparison workflow (two strategies on same golden sessions)
3. Metrics collection and comparison
```

**Deliverable**: Data-driven strategy selection and parameter tuning.

### Phase 5: Memory Programming Layer

```
1. MemoryProgrammer module (script parser + action dispatcher)
2. YAML script schema + Pydantic validation
3. LLM natural-language → script conversion
4. CLI commands (program, run, batch, debug, experiment)
5. Permission scoping (user-self vs admin-batch)
```

**Deliverable**: Users can declaratively program memories via natural language or YAML scripts, with full sandbox + version control.

---

## 12. Key Design Decisions

### D1: Canonical Storage + Pluggable Retrieval (Direction C)

Not "independent backends" (Direction A) and not "tabular as base" (current). Storage is unified (`mem_memories`), retrieval is pluggable. This means:
- Adding a new retrieval strategy doesn't require reimplementing store/profile/governance
- Switching strategy doesn't lose data
- Strategies can be compared fairly (same data, different retrieval)

### D2: Strategy key format is `type:version`

`"activation:v1"` not just `"graph"`. Allows multiple versions of the same strategy type to coexist for gradual upgrades and A/B testing.

### D3: Index Manager is optional and per-strategy

`vector:v1` needs no index tables (reads `mem_memories` directly). `activation:v1` needs `graph_nodes/edges`. Future `kg:v1` might need `knowledge_triples`. Each strategy owns its index lifecycle.

### D4: No fallback chain between strategies

Current graph→tabular fallback is an anti-pattern for comparison. Each strategy is self-contained. If `activation:v1` needs vector search as internal fallback, that's its implementation detail, not a cross-strategy dependency.

### D5: Per-user binding stored in DB, not config file

Config file is system-wide. Per-user binding needs to be in DB so it can be changed at runtime without restart.

### D6: All destructive operations snapshot first

Purge, migration, experiment commit — all create a snapshot before executing. Rollback is always possible via `RESTORE FROM SNAPSHOT`.

### D7: Backfill is the migration mechanism

Switching strategy = backfill new index from canonical storage. No data export/import. Backfill must be idempotent and incremental.

### D8: MemoryEditor is separate from MemoryWriter

`MemoryWriter.store()` is the normal write path (from conversation). `MemoryEditor.inject/correct/purge` are administrative operations with different trust levels, audit requirements, and safety guarantees.

---

## 13. Open Questions (Resolved)

| # | Question | Resolution |
|---|---|---|
| 1 | Phase 0 scope: minimal vs full? | **Full Decouple**. Direction C's core is "no wrapper". See D1. |
| 2 | Backfill performance & UX? | **Async** (background job). ~2 min for 10K memories. `index_status` field visible via API/CLI. User sees "构建加速索引中" and continues using old strategy. |
| 3 | Index garbage collection? | Keep orphaned index data **30 days** for potential switch-back, then GC. Orphaned data doesn't affect correctness. |
| 4 | Experiment concurrency? | **Yes**, multiple active experiments allowed. Only one can commit at a time — **optimistic locking** via `base_snapshot` timestamp comparison against production state. If production changed since branch point, commit fails with `ConflictError`. |
| 5 | Experiment TTL? | **7 days default**. User can extend via `mo-agent experiment extend <id>` (adds 7 days, max 30 days). Auto-cleanup job runs daily. Orphaned branches cleaned after 24h. |
| 6 | Relearn scope? | **Configurable**, default last 30 days. Full relearn available as admin operation. |
| 7 | Metrics schema? | **Standardized** — see §8.4. Core: precision@k, recall@k, response_quality_score, retrieve_latency. Strategy-specific: multi_hop_hit_rate (activation). |
| 8 | Strategy params validation? | **Pydantic schema per strategy** — see §8.5. Validated at write time. Invalid params rejected. |
| 9 | Canonical storage evolution? | Index managers subscribe to **schema version** (stored in index metadata). On mismatch, auto-trigger incremental rebuild. |

---

## 14. Future Roadmap (v2/v3)

| # | Feature | Description | Difficulty | Priority |
|---|---|---|---|---|
| 1 | Trainable Spreading Activation | Online learning layer: contrastive loss on retrieval feedback every ~1K retrievals to fine-tune edge type weights. Replaces static `TASK_EDGE_BOOST`. | Medium (PyTorch) | v2 |
| 2 | Multimodal Memory | Add `content_type` (text/image/code) + `multimodal_embedding` column to graph nodes. Enables screenshot, code snippet, log image memories. | Medium | v2 |
| 3 | Graph Visualization | `mo-agent memory graph viz --user alice --depth 3` — interactive graph visualization for debugging multi-hop failures. Graphviz or React component. | Low | v2 |
| 4 | Standard Benchmark Integration | Built-in LongMemEval / LoCoMo golden sets. One-click evaluation in experiment branch. Quantify every change as "±X% on benchmark". | Low | v2 |
| 5 | Distributed Governance | GovernanceScheduler with distributed locking (via existing `distributed_locks` table). Required for multi-instance / multi-tenant deployment. | Medium | v3 |

### Note on Item 3 (Intent Unification)

The feedback suggested connecting `task_type` to edge boost and importance scoring. This is **already implemented** in §13 of the graph-memory design:
- `TASK_EDGE_BOOST` in `activation.py` — per-task edge type weight multipliers
- `_TASK_ACTIVATION_PARAMS` in `retriever.py` — per-task iterations and anchor_k
- `TASK_IMPORTANCE_WEIGHTS` in `importance.py` — per-task importance weight overrides
- Full integration: `GraphMemoryService.retrieve(task_hint=X)` → all three layers

All 122+ tests pass including 6 E2E tests specifically for intent-driven loading.
