# Unified Selector Pipeline Design

## Problem

Currently we have **5 separate selector implementations** that are loosely integrated:

1. **SkillSelector** (`core/skills/selector.py`) - Basic keyword matching
2. **ModernSkillSelector** (`core/skills/modern_selector.py`) - LLM function calling
3. **AuditableSkillSelector** (`core/skills/auditable_selector.py`) - Audit trail + sandbox validation
4. **SelfImprovingSelector** (`core/skills/self_improving_selector.py`) - Multi-dimensional learning
5. **AgentSkillSelector** (`core/agent/selector.py`) - Wrapper that tries to integrate them

**Issues:**
- ChatLoop only uses `get_tools_schema()` - bypasses audit, learning, validation
- Each selector has its own interface and data flow
- No unified pipeline: select → audit → learn
- Learning happens separately via API calls, not integrated into selection flow
- Redundant code across implementations

## Solution: Unified Pipeline

**Single selector with 3-stage pipeline:**

```
┌─────────────────────────────────────────────────────────────┐
│                    UnifiedSkillSelector                      │
├─────────────────────────────────────────────────────────────┤
│                                                               │
│  Stage 1: SELECT                                             │
│  ├─ Keyword matching (fast path)                            │
│  ├─ Semantic search (vector similarity)                     │
│  ├─ LLM function calling (native tools)                     │
│  └─ Apply learned corrections                               │
│                                                               │
│  Stage 2: AUDIT                                              │
│  ├─ Create selection event (Git for Data snapshot)          │
│  ├─ Record candidate scores                                 │
│  ├─ Log selection reasoning                                 │
│  └─ Optional: Sandbox pre-validation                        │
│                                                               │
│  Stage 3: LEARN                                              │
│  ├─ Collect execution feedback (4 signal types)             │
│  ├─ Multi-factor scoring (accuracy/speed/cost/satisfaction) │
│  ├─ Update learned corrections                              │
│  └─ Regression gate validation                              │
│                                                               │
└─────────────────────────────────────────────────────────────┘
```

## Architecture

### Core Class

```python
class UnifiedSkillSelector:
    """Unified skill selector with select → audit → learn pipeline."""
    
    def __init__(
        self,
        db: Session,
        llm_client,
        session_id: str,
        enable_audit: bool = True,
        enable_learning: bool = True,
        learning_weights: SignalWeights | None = None,
    ):
        self.db = db
        self.llm = llm_client
        self.session_id = session_id
        
        # Stage 1: Selection engines
        self._keyword_matcher = KeywordMatcher(db)
        self._semantic_search = SemanticSearch(db, llm_client)
        self._learned_corrections = LearnedCorrections(db)
        
        # Stage 2: Audit
        self._audit_enabled = enable_audit
        self._sandbox = Sandbox(db) if enable_audit else None
        
        # Stage 3: Learning
        self._learning_enabled = enable_learning
        self._signal_collector = SignalCollector(db)
        self._learning_weights = learning_weights or SignalWeights.default()
        self._regression_gate = RegressionGate(llm_client, db)
    
    def select(
        self,
        query: str,
        context: dict | None = None,
        max_candidates: int = 5,
        validate_in_sandbox: bool = False,
    ) -> SelectionResult:
        """Main selection pipeline: select → audit → return."""
        
        # Stage 1: SELECT
        candidates = self._select_candidates(query, max_candidates)
        candidates = self._apply_learned_corrections(query, candidates)
        
        # Stage 2: AUDIT
        if self._audit_enabled:
            event = self._create_audit_event(query, candidates)
            if validate_in_sandbox:
                candidates = self._validate_in_sandbox(candidates, event.event_id)
        
        return SelectionResult(
            skills=candidates,
            event_id=event.event_id if self._audit_enabled else None,
            reasoning=self._generate_reasoning(query, candidates),
        )
    
    def record_feedback(
        self,
        event_id: str,
        signal_type: SignalType,
        signal_data: dict,
    ):
        """Record execution feedback for learning (Stage 3)."""
        if not self._learning_enabled:
            return
        
        self._signal_collector.record(
            event_id=event_id,
            signal_type=signal_type,
            signal_data=signal_data,
        )
    
    def learn(
        self,
        days: int = 7,
        signal_types: list[SignalType] | None = None,
    ) -> LearningResult:
        """Trigger learning cycle with regression gate (Stage 3)."""
        if not self._learning_enabled:
            return LearningResult(error="Learning disabled")
        
        # Collect signals
        signals = self._signal_collector.collect(days=days, types=signal_types)
        
        # Learn corrections
        corrections = self._learn_corrections(signals)
        
        # Validate with regression gate
        gate_result = self._regression_gate.validate(
            old_corrections=self._learned_corrections,
            new_corrections=corrections,
        )
        
        if gate_result.verdict == "PASS":
            self._learned_corrections.update(corrections)
        
        return LearningResult(
            learned=len(corrections),
            gate_verdict=gate_result.verdict,
            improvement_pct=gate_result.improvement_pct,
        )
```

### Selection Methods

```python
def _select_candidates(self, query: str, max_candidates: int) -> list[SkillCandidate]:
    """Stage 1: Select candidates using multiple methods."""
    
    # Method 1: Keyword matching (fast path)
    keyword_matches = self._keyword_matcher.match(query, limit=max_candidates)
    
    # Method 2: Semantic search (if available)
    if self._semantic_search.enabled:
        semantic_matches = self._semantic_search.search(query, limit=max_candidates)
        # Merge with keyword matches
        candidates = self._merge_candidates(keyword_matches, semantic_matches)
    else:
        candidates = keyword_matches
    
    # Method 3: LLM function calling (for complex queries)
    if self._should_use_llm(query):
        llm_selection = self._llm_select(query, candidates)
        candidates = llm_selection
    
    return candidates[:max_candidates]

def _apply_learned_corrections(
    self, query: str, candidates: list[SkillCandidate]
) -> list[SkillCandidate]:
    """Apply learned corrections from historical failures."""
    if not self._learning_enabled:
        return candidates
    
    return self._learned_corrections.apply(query, candidates)
```

### Audit Trail

```python
def _create_audit_event(
    self, query: str, candidates: list[SkillCandidate]
) -> SkillSelectionEvent:
    """Stage 2: Create auditable selection event."""
    
    # Create context snapshot (Git for Data)
    snapshot_id = self._create_snapshot()
    
    event = SkillSelectionEvent(
        event_id=str(uuid7()),
        session_id=self.session_id,
        user_query=query,
        context_snapshot=snapshot_id,
        available_skills=[c.to_dict() for c in candidates],
        selected_skills=[c.name for c in candidates],
        selection_method=self._get_selection_method(),
        selection_reasoning=self._generate_reasoning(query, candidates),
        candidate_scores={c.name: c.score for c in candidates},
        created_at=datetime.now(timezone.utc),
    )
    
    self.db.add(event)
    self.db.commit()
    
    return event
```

### Learning Pipeline

```python
def _learn_corrections(self, signals: list[Signal]) -> dict[str, Correction]:
    """Stage 3: Learn from multi-dimensional signals."""
    
    corrections = {}
    
    # Group signals by query pattern
    grouped = self._group_signals_by_pattern(signals)
    
    for pattern, pattern_signals in grouped.items():
        # Multi-factor scoring
        scores = self._calculate_multi_factor_scores(pattern_signals)
        
        # Generate correction if score is below threshold
        if scores.composite_score < 0.7:
            correction = Correction(
                pattern=pattern,
                wrong_skills=self._extract_wrong_skills(pattern_signals),
                correct_skills=self._extract_correct_skills(pattern_signals),
                confidence=scores.confidence,
                signal_counts=scores.signal_counts,
            )
            corrections[pattern] = correction
    
    return corrections

def _calculate_multi_factor_scores(self, signals: list[Signal]) -> MultiFactorScore:
    """Calculate weighted score across 4 dimensions."""
    
    accuracy_score = self._score_accuracy(signals)
    speed_score = self._score_speed(signals)
    cost_score = self._score_cost(signals)
    satisfaction_score = self._score_satisfaction(signals)
    
    # Weighted composite
    composite = (
        accuracy_score * self._learning_weights.accuracy +
        speed_score * self._learning_weights.speed +
        cost_score * self._learning_weights.cost +
        satisfaction_score * self._learning_weights.satisfaction
    )
    
    return MultiFactorScore(
        accuracy=accuracy_score,
        speed=speed_score,
        cost=cost_score,
        satisfaction=satisfaction_score,
        composite_score=composite,
        confidence=self._calculate_confidence(signals),
        signal_counts={s.type: len([x for x in signals if x.type == s.type]) for s in signals},
    )
```

## Migration Plan

### Phase 1: Create Unified Selector (Week 1)

1. **Create `core/skills/unified_selector.py`**
   - Implement 3-stage pipeline
   - Integrate existing components
   - Add comprehensive tests

2. **Update ChatLoop**
   ```python
   # Old
   selector = AgentSkillSelector(db, llm_client, session_id=session_id)
   tools = selector.get_tools_schema(query)
   
   # New
   selector = UnifiedSkillSelector(db, llm_client, session_id=session_id)
   result = selector.select(query, max_candidates=5)
   tools = result.to_tools_schema()
   ```

3. **Add feedback recording**
   ```python
   # After skill execution
   selector.record_feedback(
       event_id=result.event_id,
       signal_type=SignalType.EXECUTION_TIME,
       signal_data={"duration_ms": execution_time},
   )
   ```

### Phase 2: Deprecate Old Selectors (Week 2)

1. **Mark as deprecated**
   - Add deprecation warnings to old selectors
   - Update all imports to use UnifiedSkillSelector

2. **Update API endpoints**
   - `/learning/learn` → use `selector.learn()`
   - `/learning/stats` → use `selector.get_stats()`

3. **Update tests**
   - Migrate tests to UnifiedSkillSelector
   - Keep integration tests for backward compatibility

### Phase 3: Remove Old Code (Week 3)

1. **Delete deprecated files**
   - `core/skills/selector.py`
   - `core/skills/modern_selector.py`
   - `core/skills/auditable_selector.py`
   - `core/agent/selector.py`

2. **Keep only**
   - `core/skills/unified_selector.py` (main)
   - `core/skills/self_improving_selector.py` (learning engine)
   - `core/skills/regression_gate.py` (validation)
   - `core/skills/learning_signals.py` (signal types)

## Benefits

1. **Single Entry Point**: One selector class, one interface
2. **Integrated Pipeline**: Select → audit → learn in one flow
3. **Always Auditable**: Every selection creates audit trail by default
4. **Continuous Learning**: Feedback automatically feeds learning
5. **Regression Safe**: Gate validation before deploying corrections
6. **Less Code**: ~60% reduction in selector-related code
7. **Better Testing**: Single pipeline to test, not 5 separate paths

## API Changes

### Before
```python
# Multiple selectors, manual integration
selector = AgentSkillSelector(db, llm, session_id=sid)
tools = selector.get_tools_schema(query)  # No audit, no learning

# Learning is separate
selector.learn_from_failures(days=7)  # Manual trigger
```

### After
```python
# Unified selector, automatic integration
selector = UnifiedSkillSelector(db, llm, session_id=sid)

# Selection (with audit)
result = selector.select(query, max_candidates=5)
tools = result.to_tools_schema()

# Feedback (automatic learning)
selector.record_feedback(result.event_id, SignalType.WRONG_SKILL, {...})

# Learning (with gate)
selector.learn(days=7)  # Applies corrections if gate passes
```

## Configuration

```python
# Default: All features enabled
selector = UnifiedSkillSelector(db, llm, session_id=sid)

# Disable audit (testing only)
selector = UnifiedSkillSelector(db, llm, session_id=sid, enable_audit=False)

# Disable learning
selector = UnifiedSkillSelector(db, llm, session_id=sid, enable_learning=False)

# Custom learning weights
weights = SignalWeights(
    accuracy=0.5,
    speed=0.2,
    cost=0.2,
    satisfaction=0.1,
)
selector = UnifiedSkillSelector(db, llm, session_id=sid, learning_weights=weights)
```

## Testing Strategy

1. **Unit Tests**: Each stage independently
   - Selection methods (keyword, semantic, LLM)
   - Audit event creation
   - Learning correction generation

2. **Integration Tests**: Full pipeline
   - Select → audit → feedback → learn
   - Regression gate validation
   - Sandbox pre-validation

3. **Performance Tests**: Latency benchmarks
   - Selection latency (target: <100ms)
   - Audit overhead (target: <50ms)
   - Learning cycle time (target: <5s for 1000 signals)

## Success Metrics

- **Code Reduction**: 60% less selector code
- **ChatLoop Integration**: 100% of selections audited
- **Learning Coverage**: 80% of failures trigger corrections
- **Gate Pass Rate**: >90% of learned corrections pass gate
- **Selection Latency**: <100ms p95
