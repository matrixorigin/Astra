# Unified Selector Implementation Plan

## Current State Analysis

**5 Selector Classes:**
1. `SkillSelector` - Keyword matching, dependency resolution
2. `ModernSkillSelector` - LLM function calling, tools schema
3. `AuditableSkillSelector` - Wraps ModernSkillSelector + audit events
4. `SelfImprovingSelector` - Multi-dimensional learning from signals
5. `AgentSkillSelector` - Wrapper that tries to integrate 1-4

**ChatLoop Usage:**
```python
# Only uses get_tools_schema() - bypasses audit/learning
tools_schema = self.selector.get_tools_schema(query, max_candidates)
llm_result = self.llm.chat_with_tools(messages, tools=tools_schema)
```

**Problem:** Selection, audit, and learning are disconnected.

## Implementation Steps

### Step 1: Create UnifiedSkillSelector (Core)

**File:** `core/skills/unified_selector.py`

```python
"""Unified skill selector with select → audit → learn pipeline."""

from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from core.logging_config import get_logger
from core.skills.modern_selector import ModernSkillSelector
from core.skills.self_improving_selector import SelfImprovingSelector, SignalWeights
from core.skills.learning_signals import SignalType

logger = get_logger(__name__)


@dataclass
class SelectionResult:
    """Result of skill selection with audit trail."""
    skills: list[str]  # Selected skill names
    event_id: str | None  # Audit event ID
    reasoning: str  # Selection reasoning
    candidate_scores: dict[str, float]  # skill_name -> score
    
    def to_tools_schema(self, modern_selector: ModernSkillSelector) -> list[dict]:
        """Convert to OpenAI tools schema."""
        # Get full schema for selected skills
        all_tools = modern_selector.get_tools_schema("", max_candidates=100)
        return [t for t in all_tools if t["function"]["name"] in self.skills]


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
        self.db = db
        self.llm = llm_client
        self.session_id = session_id
        self.enable_audit = enable_audit
        self.enable_learning = enable_learning
        
        # Stage 1: Selection
        self.modern_selector = ModernSkillSelector(db, llm_client)
        
        # Stage 3: Learning
        if enable_learning:
            self.improving_selector = SelfImprovingSelector(
                db, llm_client, weights=learning_weights
            )
    
    def select(
        self,
        query: str,
        context: dict | None = None,
        max_candidates: int = 5,
    ) -> SelectionResult:
        """Main pipeline: select → audit → return.
        
        Args:
            query: User query
            context: Optional context
            max_candidates: Max skills to select
            
        Returns:
            SelectionResult with skills and audit trail
        """
        # Stage 1: SELECT
        # Use modern selector for LLM-based selection
        tool_calls = self.modern_selector.select_and_execute(
            query, context, max_candidates
        )
        
        # Extract skill names
        skills = [tc["function"]["name"] for tc in tool_calls]
        
        # Apply learned corrections
        if self.enable_learning and skills:
            from core.agent.selector import SkillCandidate
            candidates = [SkillCandidate(name=name) for name in skills]
            corrected = self.improving_selector.apply_learnings(query, candidates)
            skills = [c.name for c in corrected]
        
        # Stage 2: AUDIT
        event_id = None
        if self.enable_audit:
            event_id = self._create_audit_event(query, skills)
        
        return SelectionResult(
            skills=skills,
            event_id=event_id,
            reasoning=f"Selected {len(skills)} skills for query",
            candidate_scores={s: 1.0 for s in skills},  # TODO: Real scores
        )
    
    def _create_audit_event(self, query: str, skills: list[str]) -> str:
        """Create audit event for selection."""
        from api.models import SkillSelectionEvent as EventModel
        
        event_id = str(uuid7())
        event = EventModel(
            event_id=event_id,
            session_id=self.session_id,
            user_query=query,
            selected_skills=",".join(skills),
            selection_method="unified_pipeline",
            created_at=datetime.now(timezone.utc),
        )
        self.db.add(event)
        self.db.commit()
        
        logger.debug(f"Created audit event: {event_id}")
        return event_id
    
    def record_feedback(
        self,
        event_id: str,
        signal_type: SignalType,
        signal_data: dict,
    ):
        """Record execution feedback for learning."""
        if not self.enable_learning or not event_id:
            return
        
        from api.models import LearningSignal
        
        signal = LearningSignal(
            signal_id=str(uuid7()),
            selection_event_id=event_id,
            signal_type=signal_type.value,
            signal_data=signal_data,
            created_at=datetime.now(timezone.utc),
        )
        self.db.add(signal)
        self.db.commit()
        
        logger.debug(f"Recorded {signal_type.value} signal for {event_id}")
    
    def learn(
        self,
        days: int = 7,
        signal_types: list[SignalType] | None = None,
    ) -> dict[str, Any]:
        """Trigger learning cycle with regression gate."""
        if not self.enable_learning:
            return {"error": "Learning disabled"}
        
        return self.improving_selector.learn_from_failures(
            days=days, signal_types=signal_types
        )
    
    def get_stats(self) -> dict[str, Any]:
        """Get learning statistics."""
        if not self.enable_learning:
            return {"error": "Learning disabled"}
        
        return self.improving_selector.get_learning_stats()
```

### Step 2: Update ChatLoop Integration

**File:** `core/agent/chat_loop.py`

```python
# OLD CODE (lines ~150-155)
tools_schema = self.selector.get_tools_schema(
    query=user_input, max_candidates=max_candidates
)

# NEW CODE
selection_result = self.selector.select(
    query=user_input,
    context=context,
    max_candidates=max_candidates,
)
tools_schema = selection_result.to_tools_schema(self.selector.modern_selector)

# Store event_id for feedback
self._last_selection_event_id = selection_result.event_id
```

**Add feedback recording after execution:**

```python
# After tool execution (in tool use loop)
if hasattr(self.selector, 'record_feedback') and self._last_selection_event_id:
    self.selector.record_feedback(
        event_id=self._last_selection_event_id,
        signal_type=SignalType.EXECUTION_TIME,
        signal_data={"duration_ms": execution_time_ms},
    )
```

### Step 3: Update CLI Initialization

**File:** `cli/mo_agent.py`

```python
# OLD CODE (line ~69)
selector = AgentSkillSelector(db, llm_client, auditable=True, session_id=session.session_id)

# NEW CODE
from core.skills.unified_selector import UnifiedSkillSelector

selector = UnifiedSkillSelector(
    db=db,
    llm_client=llm_client,
    session_id=session.session_id,
    enable_audit=True,
    enable_learning=True,
)
```

### Step 4: Update API Endpoints

**File:** `api/routers/learning.py`

```python
# OLD CODE (line ~122)
selector = AgentSkillSelector(
    db, llm_client, session_id="learning_cycle", enable_learning=True
)

# NEW CODE
from core.skills.unified_selector import UnifiedSkillSelector

selector = UnifiedSkillSelector(
    db=db,
    llm_client=llm_client,
    session_id="learning_cycle",
    enable_learning=True,
)
```

### Step 5: Add Database Schema (if needed)

Check if `SkillSelectionEvent` table exists:

```sql
-- Should already exist from auditable_selector
CREATE TABLE IF NOT EXISTS skill_selection_events (
    event_id VARCHAR(36) PRIMARY KEY,
    session_id VARCHAR(36) NOT NULL,
    user_query TEXT NOT NULL,
    selected_skills TEXT,  -- Comma-separated
    selection_method VARCHAR(50),
    created_at TIMESTAMP NOT NULL,
    INDEX idx_session (session_id),
    INDEX idx_created (created_at)
);
```

### Step 6: Migration Script

**File:** `scripts/migrate_to_unified_selector.py`

```python
"""Migrate from AgentSkillSelector to UnifiedSkillSelector."""

import sys
from pathlib import Path

# Add project root to path
sys.path.insert(0, str(Path(__file__).parent.parent))

from api.database import get_db_session
from core.skills.unified_selector import UnifiedSkillSelector
from core.llm.client import LLMClient

def test_unified_selector():
    """Test unified selector with sample query."""
    db = next(get_db_session())
    llm = LLMClient(db=db)
    
    selector = UnifiedSkillSelector(
        db=db,
        llm_client=llm,
        session_id="test_migration",
        enable_audit=True,
        enable_learning=True,
    )
    
    # Test selection
    result = selector.select("Create a new file", max_candidates=3)
    print(f"Selected skills: {result.skills}")
    print(f"Event ID: {result.event_id}")
    
    # Test feedback
    if result.event_id:
        from core.skills.learning_signals import SignalType
        selector.record_feedback(
            event_id=result.event_id,
            signal_type=SignalType.EXECUTION_TIME,
            signal_data={"duration_ms": 150},
        )
        print("Feedback recorded")
    
    # Test learning
    stats = selector.get_stats()
    print(f"Learning stats: {stats}")
    
    print("\n✅ Migration test passed!")

if __name__ == "__main__":
    test_unified_selector()
```

## Testing Plan

### Unit Tests

**File:** `tests/unit/test_unified_selector.py`

```python
def test_select_with_audit():
    """Test selection creates audit event."""
    selector = UnifiedSkillSelector(db, llm, session_id="test", enable_audit=True)
    result = selector.select("Create file", max_candidates=3)
    
    assert result.event_id is not None
    assert len(result.skills) > 0

def test_select_without_audit():
    """Test selection without audit."""
    selector = UnifiedSkillSelector(db, llm, session_id="test", enable_audit=False)
    result = selector.select("Create file", max_candidates=3)
    
    assert result.event_id is None
    assert len(result.skills) > 0

def test_record_feedback():
    """Test feedback recording."""
    selector = UnifiedSkillSelector(db, llm, session_id="test", enable_learning=True)
    result = selector.select("Create file")
    
    selector.record_feedback(
        event_id=result.event_id,
        signal_type=SignalType.WRONG_SKILL,
        signal_data={"expected": "file_create", "actual": "file_read"},
    )
    
    # Verify signal was recorded
    signals = db.query(LearningSignal).filter_by(selection_event_id=result.event_id).all()
    assert len(signals) == 1
```

### Integration Tests

**File:** `tests/integration/test_unified_pipeline.py`

```python
def test_full_pipeline():
    """Test select → audit → feedback → learn pipeline."""
    selector = UnifiedSkillSelector(db, llm, session_id="test")
    
    # 1. Select
    result = selector.select("Create file")
    assert result.event_id is not None
    
    # 2. Record feedback
    selector.record_feedback(
        result.event_id,
        SignalType.WRONG_SKILL,
        {"expected": "file_create", "actual": "file_read"},
    )
    
    # 3. Learn
    learn_result = selector.learn(days=1)
    assert learn_result["learned"] >= 0
```

## Rollout Plan

### Week 1: Implementation
- [ ] Create `unified_selector.py`
- [ ] Add unit tests
- [ ] Test with sample queries

### Week 2: Integration
- [ ] Update ChatLoop
- [ ] Update CLI
- [ ] Update API endpoints
- [ ] Add integration tests

### Week 3: Validation
- [ ] Run full test suite
- [ ] Performance benchmarks
- [ ] Deploy to staging

### Week 4: Cleanup
- [ ] Mark old selectors as deprecated
- [ ] Update documentation
- [ ] Remove old code (if all tests pass)

## Success Criteria

- [ ] All tests pass (527+ tests)
- [ ] ChatLoop uses unified selector
- [ ] Selection latency < 100ms p95
- [ ] 100% of selections audited
- [ ] Learning cycle completes < 5s
- [ ] Code reduction: 60% less selector code

## Rollback Plan

If issues arise:
1. Revert ChatLoop changes
2. Restore AgentSkillSelector
3. Keep UnifiedSkillSelector for gradual migration
4. Fix issues and retry

## Notes

- Keep `SelfImprovingSelector` as learning engine (don't merge)
- Keep `RegressionGate` as validation layer (don't merge)
- Only merge selection + audit into unified class
- Learning remains separate but integrated via pipeline
