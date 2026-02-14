# Unified Selector Architecture - Summary

## Problem Statement

**Current state:** 5 separate selector implementations with loose integration
- `SkillSelector` - Basic keyword matching
- `ModernSkillSelector` - LLM function calling
- `AuditableSkillSelector` - Audit trail wrapper
- `SelfImprovingSelector` - Multi-dimensional learning
- `AgentSkillSelector` - Integration wrapper

**Issues:**
- ChatLoop only uses `get_tools_schema()` - bypasses audit and learning
- No unified pipeline: select → audit → learn
- Learning happens separately via API calls
- Redundant code across implementations
- ~60% of selector code is wrapper/integration logic

## Solution: Unified Pipeline

**Single selector with 3-stage pipeline:**

```
UnifiedSkillSelector
├─ Stage 1: SELECT
│  ├─ LLM function calling (ModernSkillSelector)
│  └─ Apply learned corrections (SelfImprovingSelector)
│
├─ Stage 2: AUDIT
│  └─ Create selection event with snapshot
│
└─ Stage 3: LEARN (async)
   ├─ Collect execution feedback
   ├─ Multi-factor scoring
   └─ Regression gate validation
```

## Key Changes

### Before (Current)
```python
# ChatLoop - no audit, no learning
selector = AgentSkillSelector(db, llm, session_id=sid)
tools = selector.get_tools_schema(query)  # Direct schema, bypasses pipeline

# Learning is separate
selector.learn_from_failures(days=7)  # Manual API call
```

### After (Unified)
```python
# ChatLoop - automatic audit + learning
selector = UnifiedSkillSelector(db, llm, session_id=sid)
result = selector.select(query, max_candidates=5)  # Returns SelectionResult
tools = result.to_tools_schema(selector.modern_selector)

# Feedback automatically feeds learning
selector.record_feedback(result.event_id, SignalType.WRONG_SKILL, {...})

# Learning with regression gate
selector.learn(days=7)  # Applies corrections if gate passes
```

## Architecture

### Core Class

```python
class UnifiedSkillSelector:
    """Unified selector: select → audit → learn pipeline."""
    
    def __init__(
        self,
        db: Session,
        llm_client,
        session_id: str,
        enable_audit: bool = True,
        enable_learning: bool = True,
        learning_weights: SignalWeights | None = None,
    ):
        # Stage 1: Selection
        self.modern_selector = ModernSkillSelector(db, llm_client)
        
        # Stage 3: Learning
        if enable_learning:
            self.improving_selector = SelfImprovingSelector(db, llm_client, weights)
    
    def select(self, query: str, max_candidates: int = 5) -> SelectionResult:
        """Main pipeline: select → audit → return."""
        # Stage 1: SELECT
        tool_calls = self.modern_selector.select_and_execute(query, max_candidates)
        skills = [tc["function"]["name"] for tc in tool_calls]
        
        # Apply learned corrections
        if self.enable_learning:
            skills = self.improving_selector.apply_learnings(query, skills)
        
        # Stage 2: AUDIT
        event_id = self._create_audit_event(query, skills) if self.enable_audit else None
        
        return SelectionResult(skills, event_id, reasoning, scores)
    
    def record_feedback(self, event_id: str, signal_type: SignalType, data: dict):
        """Record execution feedback for learning."""
        # Store signal for next learning cycle
    
    def learn(self, days: int = 7) -> dict:
        """Trigger learning cycle with regression gate."""
        return self.improving_selector.learn_from_failures(days)
```

### SelectionResult

```python
@dataclass
class SelectionResult:
    """Result of skill selection with audit trail."""
    skills: list[str]  # Selected skill names
    event_id: str | None  # Audit event ID (for feedback)
    reasoning: str  # Selection reasoning
    candidate_scores: dict[str, float]  # skill_name -> score
    
    def to_tools_schema(self, modern_selector) -> list[dict]:
        """Convert to OpenAI tools schema."""
        all_tools = modern_selector.get_tools_schema("", max_candidates=100)
        return [t for t in all_tools if t["function"]["name"] in self.skills]
```

## Integration Points

### ChatLoop
```python
# Selection with audit
result = self.selector.select(user_input, max_candidates=5)
tools_schema = result.to_tools_schema(self.selector.modern_selector)

# After execution
self.selector.record_feedback(
    result.event_id,
    SignalType.EXECUTION_TIME,
    {"duration_ms": execution_time},
)
```

### CLI
```python
selector = UnifiedSkillSelector(
    db=db,
    llm_client=llm_client,
    session_id=session.session_id,
    enable_audit=True,
    enable_learning=True,
)
```

### API
```python
# Learning endpoint
@router.post("/learning/learn")
def trigger_learning(days: int = 7):
    selector = UnifiedSkillSelector(db, llm, session_id="learning_cycle")
    return selector.learn(days=days)

# Stats endpoint
@router.get("/learning/stats")
def get_stats():
    selector = UnifiedSkillSelector(db, llm, session_id="stats")
    return selector.get_stats()
```

## Migration Plan

### Phase 1: Create Unified Selector (Week 1)
- Create `core/skills/unified_selector.py`
- Implement 3-stage pipeline
- Add unit tests

### Phase 2: Integrate with ChatLoop (Week 2)
- Update ChatLoop to use `selector.select()`
- Add feedback recording after execution
- Update CLI and API endpoints

### Phase 3: Deprecate Old Selectors (Week 3)
- Mark old selectors as deprecated
- Update all imports
- Run full test suite

### Phase 4: Remove Old Code (Week 4)
- Delete deprecated files (if all tests pass)
- Update documentation
- Performance benchmarks

## Benefits

1. **Single Entry Point**: One selector class, one interface
2. **Integrated Pipeline**: Select → audit → learn in one flow
3. **Always Auditable**: Every selection creates audit trail by default
4. **Continuous Learning**: Feedback automatically feeds learning
5. **Regression Safe**: Gate validation before deploying corrections
6. **Less Code**: ~60% reduction in selector-related code
7. **Better Testing**: Single pipeline to test, not 5 separate paths

## Files to Create

1. `core/skills/unified_selector.py` - Main implementation
2. `tests/unit/test_unified_selector.py` - Unit tests
3. `tests/integration/test_unified_pipeline.py` - Integration tests
4. `scripts/migrate_to_unified_selector.py` - Migration script
5. `docs/design/unified-selector-pipeline.md` - Design doc (✅ created)
6. `docs/implementation/unified-selector-migration.md` - Implementation plan (✅ created)

## Files to Update

1. `core/agent/chat_loop.py` - Use `selector.select()` instead of `get_tools_schema()`
2. `cli/mo_agent.py` - Initialize UnifiedSkillSelector
3. `api/routers/learning.py` - Use unified selector
4. `api/routers/streaming.py` - Use unified selector

## Files to Delete (Phase 4)

1. `core/skills/selector.py` - Basic selector (replaced)
2. `core/agent/selector.py` - AgentSkillSelector wrapper (replaced)

## Files to Keep

1. `core/skills/modern_selector.py` - LLM function calling engine
2. `core/skills/self_improving_selector.py` - Learning engine
3. `core/skills/regression_gate.py` - Validation layer
4. `core/skills/learning_signals.py` - Signal types
5. `core/skills/auditable_selector.py` - May keep for backward compatibility

## Success Metrics

- **Code Reduction**: 60% less selector code
- **ChatLoop Integration**: 100% of selections audited
- **Learning Coverage**: 80% of failures trigger corrections
- **Gate Pass Rate**: >90% of learned corrections pass gate
- **Selection Latency**: <100ms p95
- **Test Coverage**: All 527+ tests pass

## Next Steps

1. Review design documents
2. Create `unified_selector.py` implementation
3. Add unit tests
4. Test with sample queries
5. Update ChatLoop integration
6. Run full test suite
7. Deploy to staging
8. Cleanup old code

## Questions to Resolve

1. Should we keep `AuditableSkillSelector` for backward compatibility?
2. Should learning be synchronous or async in the pipeline?
3. What's the default behavior if audit is disabled?
4. Should we add semantic search to Stage 1?
5. How to handle sandbox pre-validation in unified pipeline?

## References

- Design doc: `docs/design/unified-selector-pipeline.md`
- Implementation plan: `docs/implementation/unified-selector-migration.md`
- Current selectors: `core/skills/`, `core/agent/selector.py`
- ChatLoop: `core/agent/chat_loop.py`
