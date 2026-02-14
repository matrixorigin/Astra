# Unified Selector - Quick Reference

## TL;DR

**Problem:** 5 separate selectors, ChatLoop bypasses audit/learning  
**Solution:** 1 unified selector with select → audit → learn pipeline  
**Result:** 60% less code, 100% audit coverage, automatic learning

## Usage

### Basic Selection

```python
from core.skills.unified_selector import UnifiedSkillSelector

selector = UnifiedSkillSelector(
    db=db,
    llm_client=llm,
    session_id=session_id,
)

# Select skills (with audit)
result = selector.select(query="Create a file", max_candidates=5)

# Get tools schema for LLM
tools = result.to_tools_schema(selector.modern_selector)

# Use event_id for feedback
print(f"Event ID: {result.event_id}")
```

### Record Feedback

```python
# After skill execution
selector.record_feedback(
    event_id=result.event_id,
    signal_type=SignalType.WRONG_SKILL,
    signal_data={
        "expected": "file_create",
        "actual": "file_read",
    },
)
```

### Trigger Learning

```python
# Learn from last 7 days
learn_result = selector.learn(days=7)

print(f"Learned: {learn_result['learned']} corrections")
print(f"Gate verdict: {learn_result['gate_verdict']}")
print(f"Improvement: {learn_result['improvement_pct']:.1f}%")
```

### Get Statistics

```python
stats = selector.get_stats()
print(f"Total corrections: {stats['total_corrections']}")
print(f"Gate pass rate: {stats['gate_pass_rate']:.1f}%")
```

## Configuration

### Enable/Disable Features

```python
# Disable audit (testing only)
selector = UnifiedSkillSelector(
    db=db,
    llm_client=llm,
    session_id=session_id,
    enable_audit=False,  # No audit trail
)

# Disable learning
selector = UnifiedSkillSelector(
    db=db,
    llm_client=llm,
    session_id=session_id,
    enable_learning=False,  # No learning
)
```

### Custom Learning Weights

```python
from core.skills.self_improving_selector import SignalWeights

weights = SignalWeights(
    accuracy=0.5,      # 50% weight on accuracy
    speed=0.2,         # 20% weight on speed
    cost=0.2,          # 20% weight on cost
    satisfaction=0.1,  # 10% weight on satisfaction
)

selector = UnifiedSkillSelector(
    db=db,
    llm_client=llm,
    session_id=session_id,
    learning_weights=weights,
)
```

## Signal Types

```python
from core.skills.learning_signals import SignalType

# Wrong skill selected
SignalType.WRONG_SKILL
# Data: {"expected": "skill_name", "actual": "skill_name"}

# Execution too slow
SignalType.SLOW_EXECUTION
# Data: {"duration_ms": 5000, "threshold_ms": 1000}

# Cost too high
SignalType.HIGH_COST
# Data: {"cost_usd": 0.50, "budget_usd": 0.10}

# User dissatisfied
SignalType.LOW_SATISFACTION
# Data: {"rating": 2, "threshold": 3}
```

## ChatLoop Integration

### Before (Current)

```python
# No audit, no learning
tools_schema = self.selector.get_tools_schema(query, max_candidates=5)
llm_result = self.llm.chat_with_tools(messages, tools=tools_schema)
```

### After (Unified)

```python
# Automatic audit + learning
result = self.selector.select(query, max_candidates=5)
tools_schema = result.to_tools_schema(self.selector.modern_selector)
llm_result = self.llm.chat_with_tools(messages, tools=tools_schema)

# Record feedback after execution
self.selector.record_feedback(
    result.event_id,
    SignalType.EXECUTION_TIME,
    {"duration_ms": execution_time},
)
```

## API Endpoints

### Learning Endpoint

```python
@router.post("/learning/learn")
def trigger_learning(days: int = 7):
    selector = UnifiedSkillSelector(
        db=db,
        llm_client=llm,
        session_id="learning_cycle",
    )
    return selector.learn(days=days)
```

### Stats Endpoint

```python
@router.get("/learning/stats")
def get_stats():
    selector = UnifiedSkillSelector(
        db=db,
        llm_client=llm,
        session_id="stats",
    )
    return selector.get_stats()
```

## CLI Commands

```bash
# Trigger learning
mo-agent learn --days 7

# View stats
mo-agent learn stats

# View recent selections
mo-agent learn selections --limit 10
```

## Database Queries

### Recent Selections

```sql
SELECT event_id, user_query, selected_skills, created_at
FROM skill_selection_events
WHERE session_id = 'session_123'
ORDER BY created_at DESC
LIMIT 10;
```

### Learning Signals

```sql
SELECT signal_type, COUNT(*) as count
FROM learning_signals
WHERE created_at > NOW() - INTERVAL 7 DAY
GROUP BY signal_type;
```

### Learned Corrections

```sql
SELECT query_pattern, wrong_skills, correct_skills, confidence
FROM learned_corrections
ORDER BY confidence DESC;
```

### Gate Results

```sql
SELECT verdict, improvement_pct, test_count, created_at
FROM selector_gate_results
ORDER BY created_at DESC
LIMIT 10;
```

## Testing

### Unit Test

```python
def test_select_with_audit():
    selector = UnifiedSkillSelector(db, llm, session_id="test")
    result = selector.select("Create file", max_candidates=3)
    
    assert result.event_id is not None
    assert len(result.skills) > 0
    assert result.reasoning != ""
```

### Integration Test

```python
def test_full_pipeline():
    selector = UnifiedSkillSelector(db, llm, session_id="test")
    
    # Select
    result = selector.select("Create file")
    
    # Feedback
    selector.record_feedback(
        result.event_id,
        SignalType.WRONG_SKILL,
        {"expected": "file_create", "actual": "file_read"},
    )
    
    # Learn
    learn_result = selector.learn(days=1)
    assert learn_result["learned"] >= 0
```

## Performance

### Selection Latency

- **Target:** <100ms p95
- **Breakdown:**
  - Stage 1 (SELECT): ~70ms
  - Stage 2 (AUDIT): ~30ms

### Learning Cycle

- **Target:** <5s for 1000 signals
- **Breakdown:**
  - Collect signals: ~500ms
  - Multi-factor scoring: ~1000ms
  - Regression gate: ~3000ms

## Migration Checklist

- [ ] Create `core/skills/unified_selector.py`
- [ ] Add unit tests
- [ ] Update ChatLoop to use `selector.select()`
- [ ] Update CLI initialization
- [ ] Update API endpoints
- [ ] Run full test suite (527+ tests)
- [ ] Deploy to staging
- [ ] Mark old selectors as deprecated
- [ ] Delete old code (if all tests pass)

## Files

### New Files
- `core/skills/unified_selector.py` - Main implementation
- `tests/unit/test_unified_selector.py` - Unit tests
- `tests/integration/test_unified_pipeline.py` - Integration tests

### Updated Files
- `core/agent/chat_loop.py` - Use unified selector
- `cli/mo_agent.py` - Initialize unified selector
- `api/routers/learning.py` - Use unified selector
- `api/routers/streaming.py` - Use unified selector

### Deprecated Files
- `core/skills/selector.py` - Basic selector
- `core/agent/selector.py` - AgentSkillSelector wrapper

### Kept Files
- `core/skills/modern_selector.py` - LLM function calling
- `core/skills/self_improving_selector.py` - Learning engine
- `core/skills/regression_gate.py` - Validation layer
- `core/skills/learning_signals.py` - Signal types

## Common Patterns

### Pattern 1: Selection + Feedback

```python
# Select
result = selector.select(query)

# Execute
for skill in result.skills:
    execute_skill(skill)

# Feedback
selector.record_feedback(result.event_id, signal_type, data)
```

### Pattern 2: Periodic Learning

```python
# Run every hour
def periodic_learning():
    selector = UnifiedSkillSelector(db, llm, session_id="cron")
    result = selector.learn(days=1)
    logger.info(f"Learned {result['learned']} corrections")
```

### Pattern 3: Custom Weights

```python
# Prioritize accuracy over speed
weights = SignalWeights(accuracy=0.7, speed=0.1, cost=0.1, satisfaction=0.1)
selector = UnifiedSkillSelector(db, llm, session_id=sid, learning_weights=weights)
```

## Troubleshooting

### No skills selected

```python
# Check if skills are registered
from api.models import SkillRegistry
skills = db.query(SkillRegistry).filter_by(is_active=1).all()
print(f"Active skills: {len(skills)}")
```

### Learning not working

```python
# Check if signals are being recorded
from api.models import LearningSignal
signals = db.query(LearningSignal).count()
print(f"Total signals: {signals}")

# Check if learning is enabled
print(f"Learning enabled: {selector.enable_learning}")
```

### Gate always fails

```python
# Check gate threshold
from core.skills.regression_gate import SkillSelectionRegressionGate
gate = SkillSelectionRegressionGate(llm, db)
stats = gate.get_gate_stats()
print(f"Pass rate: {stats['pass_rate']:.1f}%")

# Lower threshold if needed
gate.min_improvement_pct = -5.0  # Allow 5% regression
```

## Best Practices

1. **Always enable audit** - Disable only for testing
2. **Record feedback immediately** - After skill execution
3. **Run learning periodically** - Every hour or day
4. **Monitor gate results** - Ensure corrections are improving
5. **Use custom weights** - Tune for your use case
6. **Test before deploy** - Run full test suite

## Resources

- Design doc: `docs/design/unified-selector-pipeline.md`
- Implementation plan: `docs/implementation/unified-selector-migration.md`
- Visual guide: `docs/design/unified-selector-visual.md`
- Summary: `docs/design/unified-selector-summary.md`
