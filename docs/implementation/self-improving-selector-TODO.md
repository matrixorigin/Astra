# Self-Improving Selector - Implementation & Roadmap

**Last Updated:** 2026-02-14  
**Current Phase:** Phase 1 (Multi-Dimensional Learning) ✅ Complete

---

## 📖 Table of Contents

1. [Current Implementation Status](#current-implementation-status)
2. [Phase 1: Multi-Dimensional Learning - COMPLETE](#phase-1-multi-dimensional-learning---complete)
3. [Phase 0: MVP - COMPLETE](#phase-0-mvp---complete)
4. [Usage Guide](#usage-guide)
5. [Database Schema](#database-schema)
6. [Roadmap](#roadmap)
7. [Technical Debt](#technical-debt)

---

## Current Implementation Status

### ✅ Phase 1: Multi-Dimensional Learning (COMPLETE)

**Status:** 100% Complete  
**Timeline:** 2026-02-14  
**Implementation:** All features implemented and tested

**Implemented Features:**
- [x] Multi-dimensional learning signals (4 types)
- [x] Multi-factor scoring (accuracy, speed, cost, satisfaction)
- [x] Configurable weights per signal type
- [x] Learning signal extraction from failures
- [x] Regression gate validation
- [x] Complete audit trail
- [x] REST API (5 endpoints)
- [x] Runtime configuration support
- [x] Confidence decay mechanism
- [x] Error handling & edge cases

**Files:**
- `core/agent/selector.py` - Main integration (AgentSkillSelector)
- `core/skills/self_improving_selector.py` - Multi-dimensional learning logic
- `core/skills/regression_gate.py` - Validation layer
- `core/skills/learning_signals.py` - Signal types and dataclasses
- `api/routers/learning.py` - REST endpoints (5 endpoints)
- `api/models.py` - Database models (SkillSelectionEvent, SkillSelectionLearning, SkillExecutionMetric, SelectorGateResult)

**Signal Types Implemented:**
1. `WRONG_SKILL` - Incorrect skill selection (selection_correctness=0)
2. `SLOW_EXECUTION` - Execution time > 5000ms
3. `HIGH_COST` - Execution cost > $0.10
4. `LOW_SATISFACTION` - User feedback < 3 stars

**API Endpoints (5):**
- `POST /api/v1/learning/trigger` - Trigger learning cycle
- `GET /api/v1/learning/signals` - List signal types
- `GET /api/v1/learning/stats` - Get learning statistics
- `POST /api/v1/learning/feedback` - Submit feedback
- `GET /api/v1/learning/health` - Health check

---

### ✅ Phase 0: MVP (Complete)

**Status:** 100% Complete  
**Timeline:** 2026-02-14  
**Tests:** 54 passed

**Implemented:**
- [x] Single-dimension learning (wrong skill)
- [x] Batch learning with cooldown
- [x] Regression gate validation
- [x] Complete audit trail
- [x] REST API (4 endpoints)
- [x] SkillCandidate dataclass
- [x] Error handling

**Files:**
- `core/agent/selector.py`
- `core/skills/self_improving_selector.py`
- `core/skills/regression_gate.py`
- `api/routers/learning.py`

---

## Usage Guide

### Automatic Mode (Production)

```python
from core.agent.selector import AgentSkillSelector

selector = AgentSkillSelector(
    db=db,
    llm_client=llm_client,
    enable_learning=True,  # Default
    learning_cooldown_hours=1,
)

# Select skills - learnings applied automatically
tool_calls = selector.select_skills(query="Create a PR")
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
# API
POST /api/v1/learning/feedback
{
    "event_id": "evt_123",
    "feedback_type": "wrong_skill",
    "correct_skills": ["github_create_pr"],
    "satisfaction_score": 2
}
```

### Get Statistics

```bash
# CLI
mo-agent skill history --limit 20

# API
GET /api/v1/learning/stats
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
    selection_correctness TINYINT(1),
    correction_suggestion JSON,
    user_feedback_score INT,
    execution_time_ms INT,
    execution_cost FLOAT,
    created_at DATETIME
);
```

### skill_selection_learning
```sql
CREATE TABLE skill_selection_learning (
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

### selector_gate_results
```sql
CREATE TABLE selector_gate_results (
    gate_id VARCHAR(36) PRIMARY KEY,
    verdict VARCHAR(20),  -- PASS/FAIL
    new_avg_score FLOAT,
    old_avg_score FLOAT,
    improvement_pct FLOAT,
    learnings_applied INT,
    created_at DATETIME
);
```

### skill_execution_metrics
```sql
CREATE TABLE skill_execution_metrics (
    metric_id VARCHAR(36) PRIMARY KEY,
    session_id VARCHAR(36),
    skill_name VARCHAR(255),
    execution_time_ms INT,
    execution_cost FLOAT,
    success TINYINT(1),
    error_message TEXT,
    created_at DATETIME
);
```

---

## Roadmap

### 🚀 Phase 2: Semantic Learning (6-12 months)

**Priority:** MEDIUM  
**Effort:** 15 days

#### Tasks

**2.1 Embedding Infrastructure (5 days)**
- [ ] Integrate sentence-transformers
- [ ] Generate query embeddings
- [ ] Store in database (VECF32)
- [ ] Create vector index

**2.2 Semantic Matching (3 days)**
- [ ] Cosine similarity search
- [ ] Replace string matching
- [ ] Weighted application
- [ ] Fallback mechanism

**2.3 Context-Aware Learning (4 days)**
- [ ] Extract context features
- [ ] Context-aware matching
- [ ] Different learnings per context

**2.4 Testing (3 days)**
- [ ] Accuracy tests
- [ ] Performance benchmarks
- [ ] A/B test framework

**Success Metrics:**
- Semantic accuracy > 85%
- Latency overhead < 50ms
- Coverage improvement > 20%

---

### ⚡ Phase 3: Online Learning (12-18 months)

**Priority:** LOW  
**Effort:** 29 days

#### Tasks

**3.1 Online Model (10 days)**
- [ ] Online learning algorithm
- [ ] Replay buffer
- [ ] Mini-batch updates
- [ ] Model persistence

**3.2 Real-Time Feedback (5 days)**
- [ ] Observe after execution
- [ ] Calculate reward
- [ ] Background updates
- [ ] Async pipeline

**3.3 Exploration (4 days)**
- [ ] Epsilon-greedy
- [ ] Thompson sampling
- [ ] UCB algorithm

**3.4 Infrastructure (10 days)**
- [ ] Background worker
- [ ] Model versioning
- [ ] Monitoring
- [ ] Optimization

**Success Metrics:**
- Update latency < 1s
- Convergence < 100 iterations
- Throughput > 100 updates/s

---

### 🌐 Phase 4: Distributed Learning (18-24 months)

**Priority:** LOW  
**Effort:** 33 days

#### Tasks

**4.1 Distributed Coordination (10 days)**
- [ ] Redis distributed lock
- [ ] Signal collection
- [ ] Learning coordinator
- [ ] Model registry

**4.2 Model Distribution (8 days)**
- [ ] Pub/sub for updates
- [ ] Versioned storage
- [ ] Canary deployment
- [ ] A/B testing

**4.3 Federated Learning (15 days)**
- [ ] Privacy-preserving aggregation
- [ ] Differential privacy
- [ ] Cross-tenant learning

**Success Metrics:**
- Sync latency < 5s
- Consistency > 99%
- Fault tolerance (1 failure)
- Scalability (10+ instances)

---

## Technical Debt

### Current Limitations

1. **String Matching** → Semantic similarity (Phase 2)
2. **Single Machine** → Distributed (Phase 4)
3. **Batch Learning** → Online (Phase 3)
4. **Manual Feedback** → Automatic signals (Phase 3)

### Performance Optimizations

- [ ] Cache learning lookups (Redis)
- [ ] Batch database queries
- [ ] Async learning cycle
- [ ] Index optimization

**Priority:** LOW | **Effort:** 2-3 days

---

## Monitoring

### Key Metrics

```python
stats = selector.get_learning_stats()

# Learning metrics
print(f"Total: {stats['learnings']['total_learnings']}")
print(f"High confidence: {stats['learnings']['high_confidence']}")
print(f"Avg confidence: {stats['learnings']['avg_confidence']}")

# Gate metrics
print(f"Total gates: {stats['regression_gates']['total_gates']}")
print(f"Pass rate: {stats['regression_gates']['pass_rate']}")
```

### Database Queries

```sql
-- Recent failures
SELECT * FROM skill_selection_events 
WHERE selection_correctness = 0 
ORDER BY created_at DESC LIMIT 10;

-- High-confidence learnings
SELECT * FROM skill_selection_learning 
WHERE confidence >= 70 
ORDER BY confidence DESC;

-- Gate history
SELECT * FROM selector_gate_results 
ORDER BY created_at DESC LIMIT 10;
```

---

## Quick Reference

### Timeline

| Phase | Timeline | Effort | Priority | Status |
|-------|----------|--------|----------|--------|
| Phase 0: MVP | 2026-02-14 | 2 weeks | HIGH | ✅ Complete |
| Phase 1: Multi-Dimensional | 2026-02-14 | 8 days | HIGH | ✅ Complete |
| Phase 2: Semantic | 6-12 months | 15 days | MEDIUM | ⏳ Planned |
| Phase 3: Online | 12-18 months | 29 days | LOW | ⏳ Planned |
| Phase 4: Distributed | 18-24 months | 33 days | LOW | ⏳ Planned |

### Files

**Core:**
- `core/agent/selector.py` - Main integration
- `core/skills/self_improving_selector.py` - Learning logic
- `core/skills/regression_gate.py` - Validation
- `core/skills/learning_signals.py` - Signal types

**API:**
- `api/routers/learning.py` - REST endpoints

**Tests:**
- `tests/integration/test_self_improving_integration.py`
- `tests/integration/test_learning_api.py`

**Docs:**
- `docs/design/self-improving-selector-architecture.md` - Architecture
- `docs/design/learning-evolution-roadmap.md` - Evolution strategy

---

**Last Review:** 2026-02-14  
**Next Review:** 2026-05-14 (3 months)

**Goal:** Expand from single dimension to multi-factor optimization

**Priority:** HIGH  
**Effort:** Medium (2-3 weeks)  
**Dependencies:** None

### Tasks

#### 1.1 Extend Learning Signals
- [x] Define `LearningSignal` dataclass with signal types
- [x] Support `slow_execution` signal
- [x] Support `high_cost` signal
- [x] Support `low_satisfaction` signal
- [x] Database migration

**Files to modify:**
```
core/skills/self_improving_selector.py
api/models.py
migrations/008_multi_dimensional_learning.sql
```

**Estimated effort:** 3 days

#### 1.2 Multi-Factor Scoring
- [x] Implement weighted scoring (accuracy, speed, cost, satisfaction)
- [x] Configurable weights per signal type
- [x] Aggregate learnings from multiple signals

**Files to modify:**
```
core/skills/self_improving_selector.py
core/agent/selector.py
```

**Estimated effort:** 2 days

#### 1.3 API Extensions
- [x] Add `signal_types` parameter to `/api/v1/learning/trigger`
- [x] Return multi-dimensional improvement metrics
- [x] Add `/api/v1/learning/signals` endpoint (list signal types)

**Files to modify:**
```
api/routers/learning.py
tests/integration/test_learning_api.py
```

**Estimated effort:** 1 day

#### 1.4 Testing
- [x] Unit tests for each signal type
- [x] Integration tests for multi-signal learning
- [x] Regression gate tests with multi-dimensional metrics

**Estimated effort:** 2 days

**Total Phase 1 Effort:** ~8 days

---

## 🧠 Phase 2: Semantic Learning (6-12 months)

**Goal:** Upgrade from string matching to semantic understanding

**Priority:** MEDIUM  
**Effort:** High (3-4 weeks)  
**Dependencies:** Phase 1 (optional)

### Tasks

#### 2.1 Embedding Infrastructure
- [ ] Add embedding model integration (sentence-transformers)
- [ ] Generate embeddings for query patterns
- [ ] Store embeddings in database (VECF32 column)
- [ ] Create vector index for similarity search

**Files to create/modify:**
```
core/embeddings/encoder.py (new)
core/skills/semantic_learner.py (new)
migrations/009_semantic_learning.sql
```

**Estimated effort:** 5 days

#### 2.2 Semantic Matching
- [ ] Implement cosine similarity search
- [ ] Replace string matching with semantic search
- [ ] Weighted application based on similarity score
- [ ] Fallback to string matching if embedding fails

**Files to modify:**
```
core/skills/self_improving_selector.py
core/agent/selector.py
```

**Estimated effort:** 3 days

#### 2.3 Context-Aware Learning
- [ ] Extract context features (session history, user profile)
- [ ] Context-aware pattern matching
- [ ] Different learnings for different contexts

**Files to modify:**
```
core/skills/self_improving_selector.py
api/models.py
```

**Estimated effort:** 4 days

#### 2.4 Testing & Benchmarking
- [ ] Semantic matching accuracy tests
- [ ] Performance benchmarks (latency impact)
- [ ] A/B test framework (semantic vs string matching)

**Estimated effort:** 3 days

**Total Phase 2 Effort:** ~15 days

---

## ⚡ Phase 3: Online Learning (12-18 months)

**Goal:** Real-time learning instead of batch processing

**Priority:** LOW  
**Effort:** Very High (6-8 weeks)  
**Dependencies:** Phase 2

### Tasks

#### 3.1 Online Learning Model
- [ ] Implement online learning algorithm (SGD, Adam)
- [ ] Replay buffer for experience storage
- [ ] Mini-batch updates
- [ ] Model persistence and loading

**Files to create:**
```
core/learning/online_model.py (new)
core/learning/replay_buffer.py (new)
```

**Estimated effort:** 10 days

#### 3.2 Real-Time Feedback Loop
- [ ] Observe result after each execution
- [ ] Calculate reward signal
- [ ] Update model in background
- [ ] Async learning pipeline

**Files to modify:**
```
core/agent/selector.py
core/agent/executor.py
```

**Estimated effort:** 5 days

#### 3.3 Exploration Strategy
- [ ] Epsilon-greedy exploration
- [ ] Thompson sampling
- [ ] Upper confidence bound (UCB)
- [ ] Configurable exploration rate

**Files to create:**
```
core/learning/exploration.py (new)
```

**Estimated effort:** 4 days

#### 3.4 Infrastructure
- [ ] Background worker for model updates
- [ ] Model versioning and rollback
- [ ] Monitoring and alerting
- [ ] Performance optimization

**Estimated effort:** 10 days

**Total Phase 3 Effort:** ~29 days

---

## 🌐 Phase 4: Distributed Learning (18-24 months)

**Goal:** Multi-instance collaborative learning

**Priority:** LOW  
**Effort:** Very High (8-12 weeks)  
**Dependencies:** Phase 3

### Tasks

#### 4.1 Distributed Coordination
- [ ] Redis-based distributed lock
- [ ] Signal collection from all instances
- [ ] Centralized learning coordinator
- [ ] Model registry service

**Files to create:**
```
core/learning/distributed_learner.py (new)
core/learning/model_registry.py (new)
```

**Estimated effort:** 10 days

#### 4.2 Model Distribution
- [ ] Publish/subscribe for model updates
- [ ] Versioned model storage (S3/MinIO)
- [ ] Canary deployment
- [ ] A/B testing across instances

**Estimated effort:** 8 days

#### 4.3 Federated Learning (Optional)
- [ ] Privacy-preserving aggregation
- [ ] Differential privacy
- [ ] Cross-tenant learning

**Estimated effort:** 15 days

**Total Phase 4 Effort:** ~33 days

---

## 📊 Quick Reference

### Implementation Timeline

| Phase | Timeline | Effort | Priority | Status |
|-------|----------|--------|----------|--------|
| Phase 0: MVP | 2026-02-14 | 2 weeks | HIGH | ✅ Complete |
| Phase 1: Multi-Dimensional | 2026-02-14 | 8 days | HIGH | ✅ Complete |
| Phase 2: Semantic | 6-12 months | 15 days | MEDIUM | ⏳ Planned |
| Phase 3: Online | 12-18 months | 29 days | LOW | ⏳ Planned |
| Phase 4: Distributed | 18-24 months | 33 days | LOW | ⏳ Planned |

### Effort Summary

- **Total Planned Effort:** ~85 days
- **Phase 1 (High Priority):** 8 days ✅ Complete
- **Phase 2 (Medium Priority):** 15 days
- **Phase 3-4 (Low Priority):** 62 days

---

## 🔧 Technical Debt

### Current Limitations

1. **String Matching** - Replace with semantic similarity
   - Impact: Medium
   - Effort: Phase 2
   - Workaround: Works for exact/similar queries

2. **Single Machine** - No distributed coordination
   - Impact: Low (single instance sufficient for now)
   - Effort: Phase 4
   - Workaround: Manual coordination if needed

3. **Batch Learning** - Not real-time
   - Impact: Low (cooldown period mitigates)
   - Effort: Phase 3
   - Workaround: Trigger manually with --force

4. **Manual Feedback** - Requires human labeling
   - Impact: Medium
   - Effort: Phase 3 (automatic reward signals)
   - Workaround: API endpoint for feedback submission

### Performance Optimizations

- [ ] Cache learning lookups (Redis)
- [ ] Batch database queries
- [ ] Async learning cycle
- [ ] Index optimization

**Priority:** LOW  
**Effort:** 2-3 days

---

## 📝 Documentation TODO

- [ ] API usage examples (curl, Python, JavaScript)
- [ ] Deployment guide (Docker, Kubernetes)
- [ ] Monitoring guide (metrics, alerts)
- [ ] Troubleshooting guide
- [ ] Performance tuning guide
- [ ] Multi-dimensional learning tutorial (Phase 1)
- [ ] Semantic learning tutorial (Phase 2)

---

## 🧪 Testing TODO

### Coverage Gaps

- [ ] Concurrent learning cycles (race conditions)
- [ ] Large-scale learning (1000+ signals)
- [ ] Network failures during learning
- [ ] Database transaction rollback scenarios
- [ ] Model corruption recovery

### Performance Tests

- [ ] Learning cycle latency (target: <5s)
- [ ] API response time (target: <100ms)
- [ ] Database query performance
- [ ] Memory usage under load

---

## 🎯 Success Metrics

### Phase 0 (Current)
- ✅ Learning cycle completes successfully
- ✅ Regression gate prevents degradation
- ✅ API endpoints respond correctly
- ✅ All tests pass (54/54)

### Phase 1 (Multi-Dimensional)
- ✅ Support 4+ signal types
- ✅ Multi-dimensional improvement > 5%
- ✅ Learning cycle time < 10s
- ✅ API latency < 200ms

### Phase 2 (Semantic)
- [ ] Semantic matching accuracy > 85%
- [ ] Latency overhead < 50ms
- [ ] False positive rate < 10%
- [ ] Coverage improvement > 20%

### Phase 3 (Online)
- [ ] Real-time update latency < 1s
- [ ] Model convergence < 100 iterations
- [ ] Exploration overhead < 5%
- [ ] Throughput > 100 updates/s

### Phase 4 (Distributed)
- [ ] Cross-instance sync latency < 5s
- [ ] Model consistency > 99%
- [ ] Fault tolerance (1 instance failure)
- [ ] Scalability (10+ instances)

---

## 📞 Contact & Resources

**Documentation:**
- Implementation: `docs/implementation/self-improving-selector.md`
- Architecture: `docs/design/self-improving-selector-architecture.md`
- Evolution: `docs/design/learning-evolution-roadmap.md`

**Code:**
- Main: `core/agent/selector.py`
- Learning: `core/skills/self_improving_selector.py`
- API: `api/routers/learning.py`

**Tests:**
- Integration: `tests/integration/test_self_improving_integration.py`
- API: `tests/integration/test_learning_api.py`

---

**Last Review:** 2026-02-14  
**Next Review:** 2026-05-14 (3 months)
