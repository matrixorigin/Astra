# Memory System — Implementation Status

> **Last Updated**: 2026-03-15 (Memoria episodic integration plan)
> **Design Doc**: [memory/README.md](../design/memory/README.md)
> **Phase 1 Plan**: [memory-system-refactoring-2026-02-27.md](../../plans/memory-system-refactoring-2026-02-27.md)
> **Phase 2 Plan**: [memory-system-phase2-2026-02-27.md](../../plans/memory-system-phase2-2026-02-27.md)
> **Phase 3 Plan**: [memory-system-phase3-2026-02-27.md](../../plans/memory-system-phase3-2026-02-27.md)

---

## Phase 3 Changelog (2026-02-27)

| Change | Rationale |
|--------|-----------|
| **Wired `GovernanceScheduler` into production scheduler** | `scheduler.py:_dispatch()` now calls both `MemoryGovernanceEngine` (knowledge entries) and `GovernanceScheduler` (memories table) |
| **Added `run_daily_all()`** | Iterates all users for scheduled daily governance |
| **Wired `SessionSummarizer` into session close** | `session_manager.close_session()` generates full summary |
| **Wired `SessionSummarizer` into turn hooks** | `turn_hooks.run_observer()` checks incremental summary thresholds |
| **Consolidated trust tier constants** | `trust_tier_defaults()` and `TRUST_TIER_INITIAL_CONFIDENCE` canonical in `types.py`; `lifecycle.py` re-exports for backward compat |
| **Migrated `knowledge/api.py` imports** | 4 imports moved from `lifecycle.py` to `types.py` |
| **Removed dead code from `lifecycle.py`** | `_apply_confidence_decay()` (double-decay bug), `_compress_episodic_events()` (replaced by SessionSummarizer), `_run_reflector()` (no-op), duplicate constants |
| **Fixed tests** | Updated `test_lifecycle.py` (removed 3 decay tests, updated daily/rollback), removed `test_compress_episodic_writes_summary` |

## Phase 2 Changelog (2026-02-27)

| Change | Rationale |
|--------|-----------|
| **Added `TrustTier` enum (T1-T4)** | Per-tier half-life for confidence decay: T1=365d, T2=180d, T3=60d, T4=30d |
| **Added `trust_tier` column to `memories`** | VARCHAR(10), default "T3", nullable |
| **Governance frequency separation** | `run_hourly()`, `run_daily()`, `run_weekly()` with distinct responsibilities |
| **Working memory archival** | `run_hourly()` archives WORKING memories inactive > 2 hours |
| **Quarantine** | `run_daily()` deactivates memories with effective_confidence < threshold (per tier) |
| **SessionSummarizer** | Incremental summaries at turn/time thresholds, full summary on close |
| **Sensitivity audit logging** | Structured log with content_hash (no raw content) |
| **Architecture doc fixed** | 13 inconsistencies (D1-D13) resolved |

## Phase 1 Changelog (2026-02-27)

| Change | Rationale |
|--------|-----------|
| **Removed `MemoryType.EPISODIC`** | No episodic rows in `memories` table |
| **Renamed `confidence` → `initial_confidence`** | Immutable at write time; `effective_confidence()` at query time |
| **Removed `_apply_decay()` from governance** | Eliminates double-decay bug (B2) |
| **Replaced metrics singleton with DI** | `MemoryMetrics` instances via constructor |
| **Wired `embed_fn` in `turn_hooks.py`** | Fixes B1 — vector search now functional |
| **Created `sensitivity.py`** | Regex-based PII/credential filter |

---

## Memoria Episodic Integration (2026-03-15)

| Item | Status | Notes |
|------|--------|-------|
| **Episodic memory type** | 🔵 Planned | Memoria Phase 1 adds `MemoryType.EPISODIC` with metadata fields |
| **Session summary API** | 🔵 Planned | `/v1/sessions/{id}/summary` (manual trigger, full mode only) |
| **Retrieval support** | 🔵 Planned | Use Memoria `retrieve` with episodic types + explain |
| **Async task status** | 🔵 Planned | `/v1/tasks/{id}` for summary job polling |
| **Privacy control** | 🔵 Planned | `no_episodic` session metadata + scope=session on sensitive content |
| **Non-close triggers** | 🔵 Planned | Event threshold + time-based batch + idle detection + user command |

### Cross-Session Continuity Policy (Planned)

| Scenario | Behavior | Data Source | Fallback |
|----------|----------|-------------|----------|
| **Same session after relogin** | Show full chat history (or last N turns by UI paging) | `conversation_events` | None |
| **Different session after relogin** | Show recent episodic topics + key outcomes | Memoria episodic retrieval | Rule-based topic stub if episodic is empty |
| **Below-threshold sessions** | Always generate a lightweight topic stub | Session metadata + last 5-10 messages | Store as episodic with low confidence |

**Minimum recall guarantee**:
- Never return “nothing to recall” for user-facing history.
- If episodic is empty and session is short, store a **topic stub** on chat close or idle:
  - Example content: "User asked about Memoria CI status"
  - Metadata: `topic`, `session_id`, `source_event_ids`, `confidence=low`

## Module Mapping

| Component | Status | Module | Notes |
|-----------|--------|--------|-------|
| **Memory Types** | ✅ | `types.py` | `MemoryType`, `TrustTier` enums, `Memory` dataclass with `effective_confidence()` (per-tier half-life) |
| **Memory Store** | ✅ | `store.py` | CRUD + atomic supersede + `archive_working_memories()`, DI metrics |
| **Memory Model** | ✅ | `api/models/memory.py` | `initial_confidence` + `trust_tier` columns, vector + fulltext indexes |
| **Memory Retriever** | ✅ | `retriever.py` | 3-phase hybrid retrieval, per-tier decay in merge scoring, DI metrics |
| **Typed Observer** | ✅ | `typed_observer.py` | LLM extraction → sensitivity filter → embed → contradiction → store. Accepts `trust_tier` param. |
| **Sensitivity Filter** | ✅ | `sensitivity.py` | Block-only regex filter (8 patterns), structured audit logging with content_hash |
| **Profile Manager** | ✅ | `profile.py` | L0 profile synthesis + caching |
| **Memory Sandbox** | ✅ | `sandbox.py` | Zero-copy branch validation, DI metrics |
| **Provenance** | ✅ | `provenance.py` | PITR queries, diff, rollback |
| **Health** | ✅ | `health.py` | Pollution detection, storage stats, orphan cleanup |
| **Governance** | ✅ | `governance.py` | Frequency-separated: hourly (tool_result + working archival), daily (stale + quarantine), weekly (branches + snapshots) |
| **Session Summarizer** | ✅ | `session_summary.py` | Incremental (session-scoped) + full (cross-session) summaries |
| **Metrics** | ✅ | `metrics.py` | `MemoryMetrics` class (no singleton), `Timer` context manager |
| **Tiered Loader** | ✅ | `tiered_loader.py` | L0 (profile) + L1 (semantic/procedural), DI metrics |
| **Pipeline** | ✅ | `typed_pipeline.py` | extract → sandbox → persist. DI metrics. |
| **Config** | ✅ | `config.py` | Governance config with quarantine threshold, working memory stale hours, session summary thresholds |
| **Explain** | ✅ | `explain.py` | EXPLAIN ANALYZE stats for all operations |
| **Prompts** | ✅ | `prompts.py` | Observer extraction prompt (no episodic, no reflector) |

---

## Known Issues

### Bugs — Resolved ✅

| ID | Summary | Resolution |
|----|---------|------------|
| **B1** | `embed_fn` not wired in `turn_hooks.py` | ✅ Fixed — `EmbeddingService.embed_text` wired through |
| **B2** | Double-decay (governance mutates + retriever decays) | ✅ Fixed — `initial_confidence` immutable, `_apply_decay()` removed |

### Design–Implementation Gaps

| ID | Summary | Status |
|----|---------|--------|
| **G1** | Two independent retrievers | 🔵 Accepted separation |
| **G2** | Source Trust Tiers (T1-T4) | ✅ Implemented — `TrustTier` enum, per-tier half-life |
| **G3** | Working memory lifecycle | ✅ Implemented — `run_hourly()` archives stale working memories |
| **G4** | Episodic compression (90-day TTL → summary) | 🔵 Design Target — events table, not memories |
| **G5** | Pollution detection simplified | 🔵 Supersede ratio only — cascade analysis is Design Target |
| **G6** | Governance frequency separation | ✅ Implemented — hourly/daily/weekly |
| **G7** | Distributed lock not implemented | ✅ Scheduler has `distributed_locks` table + heartbeat; `GovernanceScheduler` wired in |
| **G11** | Sensitivity filter | ✅ Implemented — block-only, regex, audit logging |
| **G12** | Session summaries | ✅ Implemented + wired — `SessionSummarizer` called on session close + turn hooks |
| **G13** | Memoria session summary not wired | 🔵 Memoria backend returns None; API call pending |
| **G14** | Episodic retrieval not integrated | 🔵 Tiered loader uses Memoria search only |
| **G15** | Observe cost scales linearly per turn | 🔵 Batch observe requires Memoria batch endpoint |

---

## Next Steps

| Priority | Item | Notes |
|----------|------|-------|
| P1 | Wire Memoria session summary API | Call `/v1/sessions/{id}/summary` on session close |
| P1 | Add episodic retrieval in Tiered loader | Use Memoria retrieve with memory_types |
| P1 | Add episodic trigger policy | Event count (20-30), time window (30min), idle archive |
| P2 | Add Memoria task polling | Track summary task status for observability |
| P2 | Batch observe pipeline | Every N turns or M minutes, send last K messages; requires Memoria batch observe |
| P2 | Reflector (clustering/promotion) | Design Target — would enable episodic→semantic synthesis |
| P3 | G4: Episodic compression | Compress old `conversation_events` to summaries |
| P3 | G5: Cascade impact analysis | Trace contamination graph from polluted memories |

### Batch Observe Constraints (Multi-user Reality)

- **Low per-session frequency**: single session traffic is sparse, so batching by session alone yields limited savings.
- **Global scale benefit**: batching matters at thousands+ concurrent users; schedule should aggregate across sessions while preserving session isolation.
- **Consistency guarantees**:
  - Per-session monotonic processing with `last_observed_event_id` watermark.
  - Idempotent batch via `batch_id` to avoid duplicate memories on retries.
  - Read-your-writes via prompt cache (recent turns stay visible even if observe is delayed).
