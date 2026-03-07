# Memory Backend Coexistence Design

> tabular = flat table + vector/fulltext retrieval (current `MemoryService`)
> graph = graph-based + spreading activation + reflection (graph-memory.md)
>
> Both implement the same protocols. Zero shared internals.

---

## 1. Protocol Boundary (Already Exists)

The existing `core/memory/interfaces.py` defines three protocols that all consumers depend on:

```python
class MemoryReader(Protocol):
    def retrieve(self, user_id, query, *, session_id, ...) -> list[Memory]: ...
    def get_profile(self, user_id) -> str | None: ...

class MemoryWriter(Protocol):
    def store(self, user_id, content, *, memory_type, ...) -> Memory: ...
    def observe_turn(self, user_id, messages, ...) -> list[Memory]: ...

class MemoryAdmin(Protocol):
    def run_governance(self, user_id) -> GovernanceReport: ...
    def health_check(self, user_id) -> HealthReport: ...
```

The shared reflection engine adds one internal protocol for candidate provision:

```python
class CandidateProvider(Protocol):
    """Each backend implements this to feed the shared ReflectionEngine."""
    def get_reflection_candidates(
        self, user_id: str, *, since_hours: int = 24,
    ) -> list[ReflectionCandidate]: ...
```

Consumers (`TieredMemoryLoader`, `PromptAssembler`, `ContextScheduler`) already use `MemoryService` through these protocols. **No consumer changes needed.**

---

## 2. Module Layout

```
core/memory/
├── interfaces.py          # Shared protocols (unchanged)
├── types.py               # Shared types: Memory, MemoryType, TrustTier (unchanged)
├── config.py              # Shared config (add backend selector)
├── factory.py             # NEW: creates tabular or graph service
│
├── reflection/            # Shared reflection engine (backend-agnostic)
│   ├── __init__.py
│   ├── engine.py          # ReflectionEngine: candidate selection → LLM synthesis → persist
│   ├── importance.py      # ImportanceScorer: 4-signal heuristic scoring
│   ├── opinion.py         # OpinionEvolver: evidence-based confidence updates
│   └── prompts.py         # Reflection prompt templates (managed by PromptOptimizer)
│
├── tabular/               # Current implementation, moved here
│   ├── __init__.py
│   ├── service.py         # TabularMemoryService (renamed from MemoryService)
│   ├── store.py           # MemoryStore (unchanged)
│   ├── retriever.py       # MemoryRetriever (unchanged)
│   ├── typed_observer.py  # TypedObserver (unchanged)
│   ├── typed_pipeline.py  # run_typed_memory_pipeline (unchanged)
│   ├── governance.py      # GovernanceScheduler (unchanged)
│   ├── session_summary.py # SessionSummarizer (unchanged)
│   ├── profile.py         # ProfileManager (unchanged)
│   ├── health.py          # MemoryHealth (unchanged)
│   ├── sensitivity.py     # check_sensitivity (unchanged)
│   ├── sandbox.py         # MemorySandbox (unchanged)
│   ├── explain.py         # Stats types (unchanged)
│   ├── metrics.py         # MemoryMetrics (unchanged)
│   ├── candidates.py      # NEW: TabularCandidateProvider (feeds reflection engine)
│   └── prompts.py         # Observer prompts (unchanged)
│
├── graph/                 # Graph-based implementation (new)
│   ├── __init__.py
│   ├── service.py         # GraphMemoryService (implements same protocols)
│   ├── graph_store.py     # memory_graph_nodes table CRUD
│   ├── graph_builder.py   # Event → node + edge extraction
│   ├── in_memory_graph.py # Adjacency traversal + LRU cache
│   ├── activation.py      # Spreading activation retrieval
│   ├── graph_cache.py     # Tiered loading + 512MB LRU
│   ├── candidates.py      # NEW: GraphCandidateProvider (feeds reflection engine)
│   └── governance.py      # Graph-specific governance (orphan detection, edge pruning)
│
└── __init__.py            # Re-exports factory + protocols + shared types
```

**Key rules**:
- `tabular/` imports nothing from `graph/`. `graph/` imports nothing from `tabular/`.
- Both import from `interfaces.py`, `types.py`, `config.py`, and `reflection/`.
- `reflection/` imports only from `interfaces.py` and `types.py` — never from `tabular/` or `graph/`.
- Each backend provides a `CandidateProvider` that feeds the shared reflection engine.

---

## 3. Factory

```python
# core/memory/factory.py
from __future__ import annotations

from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from core.db_consumer import DbFactory
    from core.memory.interfaces import MemoryAdmin, MemoryReader, MemoryWriter

# Union type for the facade
type MemoryFacade = MemoryReader & MemoryWriter & MemoryAdmin


def create_memory_service(
    db_factory: DbFactory,
    *,
    backend: str = "tabular",
    llm_client: Any = None,
    embed_fn: Any = None,
    config: Any = None,
) -> Any:
    """Create memory service by backend.

    Args:
        backend: "tabular" (flat table) or "graph" (graph-based).
                 Read from config.memory_backend if not specified.
    """
    if backend == "graph":
        from core.memory.graph.service import GraphMemoryService
        return GraphMemoryService(
            db_factory, llm_client=llm_client, embed_fn=embed_fn, config=config,
        )

    from core.memory.tabular.service import TabularMemoryService
    return TabularMemoryService(
        db_factory, llm_client=llm_client, embed_fn=embed_fn, config=config,
    )
```

---

## 4. Config Addition

```python
# In core/memory/config.py — add one field
@dataclass
class MemoryGovernanceConfig:
    # ... existing fields ...
    memory_backend: str = "tabular"  # "tabular" or "graph"
```

---

## 5. Consumer Impact

**Zero changes required.** All consumers already use `MemoryService` through protocols:

| Consumer | Current Import | After Migration |
|---|---|---|
| `TieredMemoryLoader` | `from core.memory.service import MemoryService` | `from core.memory.factory import create_memory_service` |
| `PromptAssembler` | `MemoryService(self._db_factory)` | `create_memory_service(self._db_factory)` |
| `ContextScheduler` | `MemoryService(db_factory)` | `create_memory_service(db_factory)` |

The import change is mechanical — find-and-replace `MemoryService(` → `create_memory_service(`. The return type satisfies the same protocols.

---

## 6. Shared Types (No Duplication)

Both v1 and v2 return the same `Memory` dataclass:

```python
# core/memory/types.py — unchanged, shared by both versions
@dataclass
class Memory:
    memory_id: str
    user_id: str
    memory_type: MemoryType
    content: str
    initial_confidence: float
    trust_tier: TrustTier
    session_id: str | None
    observed_at: datetime
    embedding: list[float] | None
    source_event_ids: list[str] | None
    is_active: bool
```

v2 internally uses `GraphNode` for its graph structure, but converts to `Memory` at the protocol boundary. Consumers never see `GraphNode`.

---

## 7. Database Tables

| Backend | Tables | Interaction |
|---|---|---|
| tabular | `memory_entries`, `memory_index` | Existing tables, unchanged |
| graph | `memory_graph_nodes` | New table, no FK to tabular tables |

**No shared tables.** Switching backends means switching which tables are read/written. Old data remains accessible by switching back to tabular.

---

## 8. Migration Path

```
Phase 0 (now):     tabular only, MemoryService
Phase 1 (Week 1):  Move current code to tabular/, add factory, all tests pass
Phase 2 (Week 2-6): Build graph/ behind factory, tabular remains default
Phase 3 (Week 7):  Shadow mode — graph runs in parallel, results logged but not served
Phase 4 (Week 8):  A/B test — config flag switches per-user
Phase 5 (future):  graph default, tabular available as fallback
```

Phase 1 is a pure refactor — no behavior change, just file moves + factory introduction.

---

## 9. Testing Strategy

```python
# Parametrized tests run against both backends
@pytest.fixture(params=["tabular", "graph"])
def memory_service(request, db_factory):
    return create_memory_service(db_factory, backend=request.param)

def test_store_and_retrieve(memory_service):
    """Both backends must satisfy the same contract."""
    mem = memory_service.store("alice", "prefers dark mode", memory_type=MemoryType.PROFILE)
    results = memory_service.retrieve("alice", "color theme preference")
    assert any(m.memory_id == mem.memory_id for m in results)
```

graph-specific tests (graph structure, activation, reflection) live in `tests/unit/memory/graph/`.

---

## 10. Backward Compatibility Guarantee

```python
# core/memory/__init__.py — maintains all existing exports
from core.memory.factory import create_memory_service
from core.memory.tabular.service import TabularMemoryService as MemoryService  # backward compat alias

# All existing imports continue to work:
# from core.memory import MemoryService  ← still works, returns tabular
# from core.memory import MemoryStore    ← still works
# from core.memory import TypedObserver  ← still works
```

The `MemoryService` name remains as an alias to `TabularMemoryService`. New code uses `create_memory_service()`.
