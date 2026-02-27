# Memory System — Implementation Status

> **Last Updated**: 2026-02-27 (post-refactoring)  
> **Design Doc**: [memory-architecture.md](../design/memory-architecture.md)  
> **Refactoring Plan**: [memory-system-refactoring-2026-02-27.md](../../plans/memory-system-refactoring-2026-02-27.md)

---

## Refactoring Changelog (2026-02-27)

| Change | Rationale |
|--------|-----------|
| **Removed `TypedReflector`** | Operated on `MemoryType.EPISODIC` rows in `memories` table — architecture says episodic lives exclusively in `conversation_events` |
| **Removed `MemoryType.EPISODIC`** | No episodic rows should exist in `memories` table |
| **Renamed `confidence` → `initial_confidence`** | Immutable at write time; `effective_confidence()` computed at query time via exponential decay |
| **Removed `_apply_decay()` from governance** | Eliminates double-decay bug (B2). Decay is stateless and idempotent at query time |
| **Replaced metrics singleton with DI** | `MemoryMetrics` instances passed via constructor — safe for `pytest -n auto` |
| **Wired `embed_fn` in `turn_hooks.py`** | Fixes B1 — vector search and contradiction detection now functional |
| **Created `sensitivity.py`** | Regex-based PII/credential filter as pre-persist hook in Observer |
| **Simplified governance** | Only cleanup tasks remain: stale inactive, orphan branches, snapshots, tool_result TTL |

### Files Deleted
- `core/memory/typed_reflector.py`
- `tests/unit/test_memory_reflector.py`

### Files Created
- `core/memory/sensitivity.py`

---

## Module Mapping

All memory code lives in `core/memory/` (16 .py files). The `MemoryRecord` model is in `api/models/memory.py`.

| Component | Status | Module | Notes |
|-----------|--------|--------|-------|
| **Memory Types** | ✅ | `types.py` | `MemoryType` enum (profile/semantic/procedural/working/tool_result), `Memory` dataclass with `effective_confidence()` |
| **Memory Store** | ✅ | `store.py` | CRUD + atomic supersede, accepts `MemoryMetrics` via DI |
| **Memory Model** | ✅ | `api/models/memory.py` | `initial_confidence` column (immutable), vector + fulltext indexes |
| **Memory Retriever** | ✅ | `retriever.py` | 3-phase hybrid retrieval, query-time decay in SQL + merge, DI metrics |
| **Typed Observer** | ✅ | `typed_observer.py` | LLM extraction → sensitivity filter → embed → contradiction detection → store |
| **Sensitivity Filter** | ✅ | `sensitivity.py` | Regex-based PII/credential blocking (email, phone, SSN, AWS keys, private keys, tokens, passwords) |
| **Profile Manager** | ✅ | `profile.py` | L0 profile synthesis + caching, sorts by `initial_confidence` |
| **Memory Sandbox** | ✅ | `sandbox.py` | Zero-copy branch validation, DI metrics |
| **Provenance** | ✅ | `provenance.py` | PITR queries, diff, rollback |
| **Health** | ✅ | `health.py` | Pollution detection, storage stats, orphan cleanup |
| **Governance** | ✅ | `governance.py` | Cleanup-only: stale inactive, orphan branches, snapshots, tool_result TTL. No decay mutation, no reflector |
| **Metrics** | ✅ | `metrics.py` | `MemoryMetrics` class (no singleton), `Timer` context manager |
| **Tiered Loader** | ✅ | `tiered_loader.py` | L0 (profile) + L1 (semantic/procedural), DI metrics |
| **Pipeline** | ✅ | `typed_pipeline.py` | extract → sandbox → persist. Accepts `MemoryMetrics` via DI |
| **Config** | ✅ | `config.py` | `MemoryGovernanceConfig` (reflector params removed) |
| **Explain** | ✅ | `explain.py` | EXPLAIN ANALYZE stats for all operations |

### Unified `memories` Table

The `memories` table uses `memory_type` enum: `profile`, `semantic`, `procedural`, `working`, `tool_result`.

- **No `episodic` type** — episodic memory is served exclusively from `conversation_events` via `HybridRetriever`
- **`initial_confidence`** column is immutable at write time
- **`effective_confidence(t)`** = `initial_confidence × exp(-age_days / half_life)` — computed at query time only

### Integration Points

| Integration | Location | Description |
|-------------|----------|-------------|
| Observer trigger | `core/agent/turn_hooks.py` | `run_observer()` → `run_typed_memory_pipeline()` with `embed_fn` wired |
| Embed function | `core/agent/chat_loop.py` | `EmbeddingService.embed_text` passed to `TurnHooks` |
| Prompt assembly | `core/context/prompt_assembler.py` | `TieredMemoryLoader.build_section()` |
| Governance cycle | `core/memory/governance.py` | Cleanup tasks only (no decay, no reflector) |
| Two retrievers | `core/memory/retriever.py`, `core/context/hybrid_retrieval.py` | MemoryRetriever (memories) vs HybridRetriever (events) |

---

## Known Issues

### Bugs — Resolved ✅

| ID | Summary | Resolution |
|----|---------|------------|
| **B1** | `embed_fn` not wired in `turn_hooks.py` | ✅ Fixed — `EmbeddingService.embed_text` wired through `TurnHooks` → pipeline |
| **B2** | Double-decay (governance mutates + retriever decays) | ✅ Fixed — `confidence` → `initial_confidence` (immutable), `_apply_decay()` removed |

### Design–Implementation Gaps

| ID | Summary | Status |
|----|---------|--------|
| **G1** | Two independent retrievers | 🔵 Accepted separation |
| **G2** | Source Trust Tiers (T1-T4) not implemented | 🔵 Deferred |
| **G3** | Sensory buffer / working memory lifecycle not enforced | Open |
| **G4** | Episodic compression (90-day TTL → summary) | Open (events table, not memories) |
| **G5** | Pollution detection simplified to supersede ratio | Open |
| **G6** | Governance frequency not separated (hourly/daily/weekly) | Open |
| **G7** | Distributed lock not implemented | Open |
| **G11** | Sensitivity filter | ✅ Implemented — `core/memory/sensitivity.py` |
| **G12** | Session summary only on close | 🟡 Medium priority |

---

## Next Steps

| Priority | Item | Notes |
|----------|------|-------|
| P1 | G12: Incremental session summaries | Long sessions lose context without periodic consolidation |
| P2 | G2: Trust tier schema columns | Add nullable columns as prep |
| P2 | G7: Distributed governance lock | Required for multi-instance deployment |
| P2 | DB migration: `ALTER TABLE memories CHANGE confidence initial_confidence` | Column rename in production DB |
| P3 | Remaining gaps | Iterate as needed |
