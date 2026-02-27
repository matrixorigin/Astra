# Memory System — Implementation Status

> **Last Updated**: 2026-02-27  
> **Design Doc**: [memory-architecture.md](../design/memory-architecture.md)  
> **Issue Tracker**: [MEMORY_ISSUES.md](../../MEMORY_ISSUES.md)

---

## Module Mapping

All memory code lives in `core/memory/` (17 .py files). The `MemoryRecord` model is in `api/models/memory.py`.

| Component | Status | Module | Notes |
|-----------|--------|--------|-------|
| **Memory Types** | ✅ Implemented | `types.py` | MemoryType enum, Memory dataclass |
| **Memory Store** | ✅ Implemented | `store.py` | CRUD + atomic supersede |
| **Memory Model** | ✅ Implemented | `api/models/memory.py` | MemoryRecord with vector + fulltext |
| **Memory Retriever** | ⚠️ Partial | `retriever.py` | 3-phase retrieval works, but vector search inactive in production (B1) |
| **Typed Observer** | ⚠️ Partial | `typed_observer.py` | Extraction works, `embed_fn` not wired → contradiction detection inactive (B1) |
| **Typed Reflector** | ⚠️ Partial | `typed_reflector.py` | episodic→semantic promotion works, O(n²) clustering (O2) |
| **Profile Manager** | ✅ Implemented | `profile.py` | L0 profile synthesis + caching (no dedup/conflict resolution) |
| **Memory Sandbox** | ✅ Implemented | `sandbox.py` | Zero-copy branch validation |
| **Provenance** | ✅ Implemented | `provenance.py` | PITR queries, diff, rollback |
| **Health** | ✅ Implemented | `health.py` | Pollution detection (simplified: supersede ratio only) |
| **Governance** | ⚠️ Partial | `governance.py` | Single-frequency cycle, no distributed lock, decay to be replaced (B2) |
| **Metrics** | ✅ Implemented | `metrics.py` | Latency/counter tracking, `/api/v1/evaluation/memory-metrics` |
| **Session Isolation** | ✅ Implemented | — | `session_id` column, retriever supports session filtering |
| **Tiered Loader** | ✅ Implemented | `tiered_loader.py` | L0+L1 for PromptAssembler |
| **Pipeline** | ✅ Implemented | `typed_pipeline.py` | extract→sandbox→persist→reflect |
| **Config** | ✅ Implemented | `config.py` | MemoryGovernanceConfig |
| **Sensitivity Filter** | ❌ Not implemented | — | Design specifies pre-persist hook for PII/credential filtering |
| **Session Summary (incremental)** | ❌ Not implemented | — | Design specifies N-turn / 2h incremental summaries |
| **Source Trust Tiers** | ❌ Not implemented | — | Design specifies T1-T4; implementation uses flat confidence |
| **Distributed Lock** | ❌ Not implemented | — | Design specifies `distributed_locks` table; uses in-memory `_last_cycle` dict |
| **Episodic Compression** | ❌ Not implemented | — | Design specifies 90-day TTL → summary |
| **Revalidation Cycle** | ❌ Not implemented | — | Design specifies weekly T1 source re-fetch |

### Unified memories Table

The design doc describes a multi-table schema (`knowledge_entries`, `knowledge_entry_sources`, `agent_scratchpad`) for pedagogical clarity. The actual implementation uses a **unified `memories` table** with a `memory_type` enum (`profile`/`episodic`/`semantic`/`procedural`/`working`/`tool_result`).

> **Note on `episodic`**: The `MemoryType.EPISODIC` enum value exists for backward
> compatibility, but no new episodic rows are written to `memories`. Episodic
> memory is served exclusively from `conversation_events` via `HybridRetriever`.
> Existing episodic rows should be deactivated during migration (set `is_active=0`).

### MatrixOne-Native Capabilities Used

- **PITR**: `create pitr`, `{timestamp = '...'}` reads, `restore from pitr`
- **Snapshot**: `create snapshot`, `{snapshot = '...'}` reads, `restore from snapshot`
- **Branch**: `data branch create table`, `data branch diff`, `data branch merge`, `data branch delete table`
- **Vector**: `L2_DISTANCE()` in SQL
- **Fulltext**: `MATCH() AGAINST()` with NGRAM parser
- **HTAP**: Real-time analytics on transactional data

### Integration Points

| Integration | Location | Description |
|-------------|----------|-------------|
| Observer trigger | `core/agent/turn_hooks.py:116` | `run_observer()` → `run_typed_memory_pipeline()` in daemon thread |
| Prompt assembly | `core/context/prompt_assembler.py` | `_build_memory()` → `TieredMemoryLoader.build_section()` |
| Governance cycle | `core/memory/governance.py` | Runs Reflector + decay + health |
| Two retrievers | `core/memory/retriever.py`, `core/context/hybrid_retrieval.py` | MemoryRetriever (memories) vs HybridRetriever (events) |

### Legacy System (v1) — Removed

The old system (`Observer` → `Observation` model, `SessionContinuity`) has been removed. All code now uses the v2 typed memory system.

---

## Known Issues

### Bugs (Accepted Fix Plans)

| ID | Severity | Summary | Fix Plan |
|----|----------|---------|----------|
| **B1** | 🔴 P0 | `embed_fn` not passed in `turn_hooks.run_observer()` | Short-term: `TurnHooks.__init__` creates `EmbeddingService`, passes `svc.embed_text` as `embed_fn` to `run_typed_memory_pipeline()` (single call-site change in `turn_hooks.py`). Long-term: async `MemoryEmbeddingWorker` (see O3). Owner: TBD. |
| **B2** | 🔴 P0 | Confidence decay double-decay | Rename `confidence` → `initial_confidence`. Compute `effective_confidence` at query time. Remove `_apply_decay()`. |

### Design–Implementation Gaps

| ID | Summary | Status |
|----|---------|--------|
| **G1** | Two independent retrievers | 🔵 Accepted separation — boundary rule documented. Open: no unified entry point, scoring weights differ. |
| **G2** | Source Trust Tiers (T1-T4) not implemented | 🔵 Deferred — single half_life sufficient for now. Schema columns (`trust_tier`, `verified_at`) to be added as nullable. |
| **G3** | Sensory buffer / working memory lifecycle not enforced | Open |
| **G4** | Episodic compression (90-day TTL → summary) not implemented | Open |
| **G5** | Pollution detection simplified to supersede ratio | Open |
| **G6** | Governance frequency not separated (hourly/daily/weekly) | Open |
| **G7** | Distributed lock not implemented | Open |
| **G8** | Knowledge graph not in MemoryRetriever | Open (by design — graph stays in HybridRetriever) |
| **G9** | Revalidation cycle not implemented | Open |
| **G10** | Python UDF not used | Open |
| **G11** | Sensitivity filter not implemented | 🔴 High priority — PII/credential leakage risk |
| **G12** | Session summary only on close | 🟡 Medium priority — long sessions need incremental summaries |

### Architecture Optimizations (Accepted)

| ID | Summary | Status |
|----|---------|--------|
| **O1** | Remove `episodic` from `memories` table | ✅ Adopted. Enum retained for compat, no new rows. Migration: `is_active=0`. |
| **O2** | Replace O(n²) Reflector clustering with DB-side L2_DISTANCE | ✅ Design accepted — O(n) DB queries via IVF-flat |
| **O3** | Async embedding for `memories` | ✅ Accepted as long-term fix for B1 |
| **O4** | Sandbox validation needs better quality metrics | Open |
| **O5** | Observer LLM call needs token budget cap | Open |
| **O6** | Profile synthesis needs dedup, conflict resolution | Open |
| **O7** | Per-user memory quota to prevent unbounded growth | Open |
| **O8** | IVF-flat index rebuild strategy for data drift | Open |

---

## Priority

| Priority | Item | Status | Rationale |
|----------|------|--------|-----------|
| P0 | B1 (embed_fn) | 🔧 Fix plan accepted | Vector retrieval and contradiction detection fully broken |
| P0 | B2 (double-decay) | 🔧 Fix plan accepted | Data correctness bug |
| P0 | G11 (sensitivity filter) | 🔧 Design complete | PII/credential leakage risk — must implement this week |
| P1 | G12 (incremental summaries) | 🔧 Design complete | Long sessions lose context without periodic consolidation |
| P1 | O1 (remove episodic) | ✅ Adopted | Eliminate data duplication |
| P1 | G1 (two retrievers) | 🔵 Accepted separation | Boundary clear, unified entry point deferred |
| P2 | G2 (trust tiers schema) | 🔧 Add nullable columns | Zero-cost prep for future implementation |
| P2 | O2 (Reflector clustering) | ✅ Adopted | Foreseeable performance issue |
| P2 | O3 (async embedding) | ✅ Adopted (B1 long-term) | Write performance |
| P2 | G7 (distributed lock) | Open | Required for multi-instance deployment |
| P3 | Remaining | Open | Iterate as needed |

---

## This Week's Tasks

Based on the priority above, the following should be completed this week:

### Must Do (P0)

1. **B1 Fix**: Wire `embed_fn` through `turn_hooks.py`
   - File: `core/agent/turn_hooks.py`
   - Change: Create `EmbeddingService` in `__init__`, pass `svc.embed_text` to `run_typed_memory_pipeline()`
   - Test: Verify memories have non-null embeddings after Observer runs

2. **B2 Fix**: Query-time confidence decay
   - Files: `api/models/memory.py`, `core/memory/retriever.py`, `core/memory/governance.py`
   - Changes: Rename column, update retriever SQL, remove `_apply_decay()`
   - Test: Verify effective_confidence decreases over time without governance mutation

3. **G11 Implement**: Sensitivity filter pre-persist hook
   - File: `core/memory/typed_observer.py` (new method `_filter_sensitive()`)
   - Changes: Add classification + redaction before `persist_with_contradiction_check()`
   - Test: Verify PII is redacted, credentials are discarded

### Should Do (P1)

4. **G12 Implement**: Incremental session summaries
   - Files: `core/memory/typed_reflector.py`, `core/agent/turn_hooks.py`
   - Changes: Add `generate_incremental_summary()`, call from `post_turn()` on threshold
   - Test: Verify summaries generated every 50 turns or 2 hours

### Nice to Have (P2)

5. **G2 Schema**: Add trust tier columns
   - Migration: `ALTER TABLE memories ADD COLUMN trust_tier TINYINT DEFAULT NULL`
   - No code changes needed — columns are nullable and ignored
