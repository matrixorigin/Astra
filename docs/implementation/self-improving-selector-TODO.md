# Self-Improving Selector - Implementation & Roadmap

**Last Updated:** 2026-02-20
**Current Phase:** Phase 2 (Semantic Learning) ✅ Complete

---

## 📖 Table of Contents

1. [Current Implementation Status](#current-implementation-status)
2. [Usage Guide](#usage-guide)
3. [Database Schema](#database-schema)
4. [Roadmap](#roadmap)
5. [Technical Debt](#technical-debt)

---

## Current Implementation Status

### ✅ Phase 2: Semantic Learning (COMPLETE)

**Status:** 100% Complete
**Timeline:** 2026-02-20

**Implemented Features:**
- [x] In-memory cosine similarity index (`core/skills/skill_index.py`)
- [x] Semantic retrieval as primary path; keyword fallback when no embed_fn
- [x] Real token budget control (replaces hardcoded constants)
- [x] Progressive disclosure: Tier 1 (index) → Tier 2 (full schema, budget-gated)
- [x] `embed_fn` auto-resolved from `EmbeddingService` in pipeline

**Files:**
- `core/skills/pipeline.py` — unified pipeline entry point (`SkillPipeline`)
- `core/skills/modern_selector.py` — semantic retrieval + budget control
- `core/skills/skill_index.py` — cosine similarity index
- `core/skills/self_improving_selector.py` — multi-dimensional learning logic
- `core/evaluation/regression_gate.py` — unified validation layer
- `core/skills/learning_signals.py` — signal types and dataclasses
- `api/routers/learning.py` — REST endpoints (5 endpoints)

---

### ✅ Phase 1: Multi-Dimensional Learning (COMPLETE)

**Status:** 100% Complete
**Timeline:** 2026-02-14

**Implemented Features:**
- [x] Multi-dimensional learning signals (4 types)
- [x] Multi-factor scoring (accuracy, speed, cost, satisfaction)
- [x] Configurable weights per signal type
- [x] Regression gate validation (unified `RegressionGate`)
- [x] Complete audit trail (`skill_selection_events`)
- [x] REST API (5 endpoints)
- [x] Confidence decay mechanism

**Signal Types:**
1. `WRONG_SKILL` — Incorrect skill selection
2. `SLOW_EXECUTION` — Execution time > 5000ms
3. `HIGH_COST` — Execution cost > $0.10
4. `LOW_SATISFACTION` — User feedback < 3 stars

**API Endpoints:**
- `POST /api/v1/learning/trigger`
- `GET /api/v1/learning/signals`
- `GET /api/v1/learning/stats`
- `POST /api/v1/learning/feedback`
- `GET /api/v1/learning/health`

---

### ✅ Phase 0: MVP (Complete)

**Status:** 100% Complete
**Timeline:** 2026-02-14

---

## Usage Guide

### Production Entry Point

```python
from core.skills.pipeline import SkillPipeline

pipeline = SkillPipeline(db=db, llm_client=llm_client)

# Select skills — semantic retrieval + learning corrections + audit
result = pipeline.get_tools_schema(query="Create a PR", session_id=session_id)
# result.tools: OpenAI tools schema
# result.retrieval_method: "semantic" or "keyword"
# result.event_id: audit event ID

# After execution, record feedback
pipeline.record_feedback(result.event_id, SignalType.EXECUTION_TIME, {"ms": 150})
```

### Manual Learning Trigger

```bash
# CLI
mo-agent skill learn --days 7
mo-agent skill learn --days 7 --force  # Bypass cooldown

# API
POST /api/v1/learning/trigger
{
    "days": 7,
    "force": false,
    "signal_types": ["wrong_skill", "slow_execution", "high_cost", "low_satisfaction"]
}
```

### Submit Feedback

```bash
POST /api/v1/learning/feedback
{
    "event_id": "evt_123",
    "feedback_type": "wrong_skill",
    "correct_skills": ["github_create_pr"],
    "satisfaction_score": 2
}
```

---

## Database Schema

### skill_selection_events
```sql
CREATE TABLE skill_selection_events (
    event_id VARCHAR(36) PRIMARY KEY,
    session_id VARCHAR(36),
    user_query TEXT,
    selected_skills JSON,
    selection_method VARCHAR(50),  -- "semantic" or "keyword"
    created_at DATETIME
);
```

### skill_selection_learning (selector_learnings)
```sql
CREATE TABLE selector_learnings (
    learning_id VARCHAR(36) PRIMARY KEY,
    query_pattern VARCHAR(255),
    wrong_skills JSON,
    correct_skills JSON,
    confidence FLOAT,
    evidence_count INT DEFAULT 1,
    applied_count INT DEFAULT 0,
    signal_type VARCHAR(50),
    target_metrics JSON,
    created_at DATETIME,
    updated_at DATETIME
);
```

### gate_results
```sql
CREATE TABLE gate_results (
    gate_id VARCHAR(36) PRIMARY KEY,
    change_type VARCHAR(50),
    change_id VARCHAR(255),
    verdict VARCHAR(20),  -- PASS/FAIL
    metrics JSON,
    reason TEXT,
    created_at DATETIME
);
```

### skill_learning_signals
```sql
CREATE TABLE skill_learning_signals (
    signal_id VARCHAR(36) PRIMARY KEY,
    selection_event_id VARCHAR(36),
    signal_type VARCHAR(50),
    signal_data JSON,
    created_at DATETIME
);
```

---

## Roadmap

### ⚡ Phase 3: Online Learning (12-18 months)

**Priority:** LOW | **Effort:** ~29 days | **Dependencies:** Phase 2 ✅

#### Tasks

**3.1 Online Learning Model (10 days)**
- [ ] Online learning algorithm (SGD/Adam)
- [ ] Replay buffer
- [ ] Mini-batch updates + model persistence

**3.2 Real-Time Feedback Loop (5 days)**
- [ ] Per-execution reward signal
- [ ] Async background update pipeline

**3.3 Exploration Strategy (4 days)**
- [ ] Epsilon-greedy / Thompson sampling / UCB
- [ ] Configurable exploration rate

**3.4 Infrastructure (10 days)**
- [ ] Background worker, model versioning, monitoring

**Success Metrics:**
- Update latency < 1s
- Convergence < 100 iterations
- Throughput > 100 updates/s

---

### 🌐 Phase 4: Distributed Learning (18-24 months)

**Priority:** LOW | **Effort:** ~33 days | **Dependencies:** Phase 3

#### Tasks

**4.1 Distributed Coordination (10 days)**
- [ ] Redis distributed lock, signal collection, learning coordinator

**4.2 Model Distribution (8 days)**
- [ ] Pub/sub updates, versioned storage, canary deployment

**4.3 Federated Learning (15 days)**
- [ ] Privacy-preserving aggregation, differential privacy

**Success Metrics:**
- Sync latency < 5s, consistency > 99%, 10+ instances

---

## Technical Debt

1. **Embedding cache** — `SkillIndex` re-embeds on every `build()` call. Add Redis/LRU cache when skill count > 20.
2. **Batch learning** → Online (Phase 3)
3. **Single machine** → Distributed (Phase 4)

---

## Quick Reference

### Timeline

| Phase | Timeline | Status |
|-------|----------|--------|
| Phase 0: MVP | 2026-02-14 | ✅ Complete |
| Phase 1: Multi-Dimensional | 2026-02-14 | ✅ Complete |
| Phase 2: Semantic | 2026-02-20 | ✅ Complete |
| Phase 3: Online | 12-18 months | ⏳ Planned |
| Phase 4: Distributed | 18-24 months | ⏳ Planned |

### Key Files

| File | Role |
|------|------|
| `core/skills/pipeline.py` | Unified entry point (`SkillPipeline`) |
| `core/skills/modern_selector.py` | Semantic retrieval + budget control |
| `core/skills/skill_index.py` | Cosine similarity index |
| `core/skills/self_improving_selector.py` | Multi-dimensional learning |
| `core/evaluation/regression_gate.py` | Unified regression gate |
| `core/skills/learning_signals.py` | Signal types |
| `api/routers/learning.py` | REST endpoints |

### Tests

| File | Coverage |
|------|----------|
| `tests/unit/test_progressive_disclosure.py` | SkillIndex, budget, semantic/keyword, fallback |
| `tests/integration/test_self_improving_integration.py` | Learning cycle, gate |
| `tests/integration/test_learning_api.py` | REST endpoints |
| `tests/integration/test_feedback_buffer.py` | Feedback buffering |

---

**Last Review:** 2026-02-20
**Next Review:** 2026-05-20

