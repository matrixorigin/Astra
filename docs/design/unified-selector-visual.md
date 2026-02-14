# Unified Selector Architecture - Visual Guide

## Current Architecture (5 Selectors)

```
┌─────────────────────────────────────────────────────────────────┐
│                         ChatLoop                                 │
│                                                                   │
│  selector.get_tools_schema(query) ──────────────────────┐       │
│                                                           │       │
│  llm.chat_with_tools(messages, tools=tools_schema)       │       │
│                                                           │       │
│  # No audit, no learning, no feedback loop               │       │
└───────────────────────────────────────────────────────────┼───────┘
                                                            │
                                                            ▼
┌─────────────────────────────────────────────────────────────────┐
│                    AgentSkillSelector                            │
│  (Wrapper - just delegates to other selectors)                  │
│                                                                   │
│  get_tools_schema() ──────────────────────────────────┐         │
│                                                         │         │
└─────────────────────────────────────────────────────────┼─────────┘
                                                          │
                                                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                  AuditableSkillSelector                          │
│  (Wrapper - adds audit events)                                  │
│                                                                   │
│  modern_selector.get_tools_schema() ──────────────────┐         │
│                                                         │         │
└─────────────────────────────────────────────────────────┼─────────┘
                                                          │
                                                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                   ModernSkillSelector                            │
│  (LLM function calling)                                         │
│                                                                   │
│  rule_selector.select_skills() ────────────────────────┐        │
│                                                         │         │
└─────────────────────────────────────────────────────────┼─────────┘
                                                          │
                                                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                      SkillSelector                               │
│  (Keyword matching, dependency resolution)                      │
│                                                                   │
│  SELECT * FROM skill_registry WHERE is_active = 1               │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                  SelfImprovingSelector                           │
│  (Separate - called via API, not integrated)                    │
│                                                                   │
│  learn_from_failures(days=7)  ← Manual API call                 │
│  apply_learnings()            ← Not used in ChatLoop            │
└─────────────────────────────────────────────────────────────────┘

**Problems:**
- 5 layers of wrappers
- ChatLoop bypasses audit and learning
- No feedback loop
- Learning is manual, not automatic
```

## New Architecture (Unified Pipeline)

```
┌─────────────────────────────────────────────────────────────────┐
│                         ChatLoop                                 │
│                                                                   │
│  result = selector.select(query, max_candidates=5)              │
│  tools = result.to_tools_schema(selector.modern_selector)       │
│                                                                   │
│  llm.chat_with_tools(messages, tools=tools)                     │
│                                                                   │
│  # After execution                                               │
│  selector.record_feedback(result.event_id, signal_type, data)   │
└───────────────────────────────────────────────────────────┬──────┘
                                                            │
                                                            ▼
┌─────────────────────────────────────────────────────────────────┐
│                   UnifiedSkillSelector                           │
│                                                                   │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ Stage 1: SELECT                                            │ │
│  │                                                            │ │
│  │  1. LLM function calling (ModernSkillSelector)            │ │
│  │     ├─ Get candidate skills from registry                 │ │
│  │     ├─ Generate tools schema                              │ │
│  │     └─ LLM selects best skills                            │ │
│  │                                                            │ │
│  │  2. Apply learned corrections (SelfImprovingSelector)     │ │
│  │     ├─ Check if query matches learned patterns            │ │
│  │     ├─ Remove wrong skills                                │ │
│  │     └─ Add correct skills                                 │ │
│  └────────────────────────────────────────────────────────────┘ │
│                           │                                      │
│                           ▼                                      │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ Stage 2: AUDIT                                             │ │
│  │                                                            │ │
│  │  1. Create selection event                                │ │
│  │     ├─ event_id = uuid7()                                 │ │
│  │     ├─ session_id, user_query                             │ │
│  │     ├─ selected_skills, candidate_scores                  │ │
│  │     └─ context_snapshot (Git for Data)                    │ │
│  │                                                            │ │
│  │  2. Store to database                                     │ │
│  │     └─ INSERT INTO skill_selection_events                 │ │
│  └────────────────────────────────────────────────────────────┘ │
│                           │                                      │
│                           ▼                                      │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ Return SelectionResult                                     │ │
│  │  ├─ skills: list[str]                                     │ │
│  │  ├─ event_id: str (for feedback)                          │ │
│  │  ├─ reasoning: str                                        │ │
│  │  └─ candidate_scores: dict[str, float]                    │ │
│  └────────────────────────────────────────────────────────────┘ │
│                                                                   │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ record_feedback(event_id, signal_type, data)              │ │
│  │                                                            │ │
│  │  Store signal for learning:                               │ │
│  │  ├─ WRONG_SKILL: Expected vs actual skill                 │ │
│  │  ├─ SLOW_EXECUTION: Duration > threshold                  │ │
│  │  ├─ HIGH_COST: Cost > budget                              │ │
│  │  └─ LOW_SATISFACTION: User rating < 3                     │ │
│  └────────────────────────────────────────────────────────────┘ │
│                           │                                      │
│                           ▼                                      │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ Stage 3: LEARN (async, triggered periodically)            │ │
│  │                                                            │ │
│  │  1. Collect signals (last N days)                         │ │
│  │     └─ SELECT * FROM learning_signals WHERE ...           │ │
│  │                                                            │ │
│  │  2. Multi-factor scoring                                  │ │
│  │     ├─ Accuracy score (wrong skill signals)               │ │
│  │     ├─ Speed score (slow execution signals)               │ │
│  │     ├─ Cost score (high cost signals)                     │ │
│  │     └─ Satisfaction score (low rating signals)            │ │
│  │                                                            │ │
│  │  3. Generate corrections                                  │ │
│  │     ├─ Group signals by query pattern                     │ │
│  │     ├─ Identify wrong skills to remove                    │ │
│  │     └─ Identify correct skills to add                     │ │
│  │                                                            │ │
│  │  4. Regression gate validation                            │ │
│  │     ├─ Test on golden queries                             │ │
│  │     ├─ Compare old vs new selector                        │ │
│  │     └─ Deploy if improvement > 0%                         │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘

**Benefits:**
- Single entry point
- Automatic audit trail
- Integrated learning pipeline
- Feedback loop built-in
- 60% less code
```

## Data Flow

### Selection Flow

```
User Query
    │
    ▼
┌─────────────────────────────────────┐
│ UnifiedSkillSelector.select()       │
└─────────────────────────────────────┘
    │
    ├─► Stage 1: SELECT
    │   ├─ ModernSkillSelector.select_and_execute()
    │   │  └─ Returns: [{"function": {"name": "file_create", ...}}]
    │   │
    │   └─ SelfImprovingSelector.apply_learnings()
    │      └─ Returns: [SkillCandidate("file_create", confidence=0.95)]
    │
    ├─► Stage 2: AUDIT
    │   └─ Create SkillSelectionEvent
    │      ├─ event_id: "01234567-89ab-cdef-0123-456789abcdef"
    │      ├─ session_id: "session_123"
    │      ├─ selected_skills: "file_create,file_write"
    │      └─ created_at: 2026-02-15T00:00:00Z
    │
    └─► Return SelectionResult
        ├─ skills: ["file_create", "file_write"]
        ├─ event_id: "01234567-89ab-cdef-0123-456789abcdef"
        ├─ reasoning: "Selected 2 skills for file creation"
        └─ candidate_scores: {"file_create": 0.95, "file_write": 0.85}
```

### Feedback Flow

```
Skill Execution
    │
    ▼
┌─────────────────────────────────────┐
│ selector.record_feedback()          │
│  ├─ event_id: "01234567..."         │
│  ├─ signal_type: WRONG_SKILL        │
│  └─ signal_data: {                  │
│      "expected": "file_create",     │
│      "actual": "file_read"          │
│    }                                 │
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Store LearningSignal                │
│  ├─ signal_id: "signal_123"         │
│  ├─ selection_event_id: "01234567"  │
│  ├─ signal_type: "WRONG_SKILL"      │
│  ├─ signal_data: {...}              │
│  └─ created_at: 2026-02-15T00:00:00Z│
└─────────────────────────────────────┘
    │
    ▼
┌─────────────────────────────────────┐
│ Accumulate signals for learning     │
└─────────────────────────────────────┘
```

### Learning Flow

```
Periodic Trigger (or API call)
    │
    ▼
┌─────────────────────────────────────┐
│ selector.learn(days=7)              │
└─────────────────────────────────────┘
    │
    ├─► Collect signals
    │   └─ SELECT * FROM learning_signals
    │      WHERE created_at > NOW() - INTERVAL 7 DAY
    │
    ├─► Multi-factor scoring
    │   ├─ Accuracy: 0.6 (40% wrong skills)
    │   ├─ Speed: 0.8 (20% slow)
    │   ├─ Cost: 0.9 (10% high cost)
    │   └─ Satisfaction: 0.7 (30% low rating)
    │   └─ Composite: 0.75 (weighted average)
    │
    ├─► Generate corrections
    │   └─ Pattern: "create.*file"
    │      ├─ Remove: ["file_read", "file_delete"]
    │      └─ Add: ["file_create", "file_write"]
    │
    ├─► Regression gate
    │   ├─ Test on 20 golden queries
    │   ├─ Old selector: 75% accuracy
    │   ├─ New selector: 85% accuracy
    │   └─ Improvement: +10% → PASS
    │
    └─► Deploy corrections
        └─ UPDATE learned_corrections SET ...
```

## Code Comparison

### Before (Current)

```python
# ChatLoop - no audit, no learning
class ChatLoop:
    def run_step(self, user_input, session_id, user_id):
        # Get tools schema (bypasses audit/learning)
        tools_schema = self.selector.get_tools_schema(
            query=user_input,
            max_candidates=5
        )
        
        # Call LLM
        llm_result = self.llm.chat_with_tools(
            messages=messages,
            tools=tools_schema,
        )
        
        # Execute tools
        for tool_call in llm_result.get("tool_calls", []):
            result = self.executor.execute(tool_call)
        
        # No feedback, no learning
        return final_response

# Learning is separate (manual API call)
@router.post("/learning/learn")
def trigger_learning(days: int = 7):
    selector = AgentSkillSelector(db, llm, enable_learning=True)
    return selector.learn_from_failures(days=days)
```

### After (Unified)

```python
# ChatLoop - automatic audit + learning
class ChatLoop:
    def run_step(self, user_input, session_id, user_id):
        # Select with audit
        result = self.selector.select(
            query=user_input,
            max_candidates=5
        )
        
        # Convert to tools schema
        tools_schema = result.to_tools_schema(
            self.selector.modern_selector
        )
        
        # Call LLM
        llm_result = self.llm.chat_with_tools(
            messages=messages,
            tools=tools_schema,
        )
        
        # Execute tools
        for tool_call in llm_result.get("tool_calls", []):
            start_time = time.time()
            result = self.executor.execute(tool_call)
            execution_time = (time.time() - start_time) * 1000
            
            # Record feedback (automatic learning)
            self.selector.record_feedback(
                event_id=result.event_id,
                signal_type=SignalType.EXECUTION_TIME,
                signal_data={"duration_ms": execution_time},
            )
        
        return final_response

# Learning is integrated (still accessible via API)
@router.post("/learning/learn")
def trigger_learning(days: int = 7):
    selector = UnifiedSkillSelector(db, llm, session_id="learning_cycle")
    return selector.learn(days=days)
```

## Database Schema

```sql
-- Selection events (Stage 2: AUDIT)
CREATE TABLE skill_selection_events (
    event_id VARCHAR(36) PRIMARY KEY,
    session_id VARCHAR(36) NOT NULL,
    user_query TEXT NOT NULL,
    selected_skills TEXT,  -- Comma-separated
    selection_method VARCHAR(50),
    candidate_scores JSON,  -- {"skill_name": score}
    context_snapshot VARCHAR(36),  -- Git for Data snapshot ID
    created_at TIMESTAMP NOT NULL,
    INDEX idx_session (session_id),
    INDEX idx_created (created_at)
);

-- Learning signals (Stage 3: LEARN)
CREATE TABLE learning_signals (
    signal_id VARCHAR(36) PRIMARY KEY,
    selection_event_id VARCHAR(36) NOT NULL,
    signal_type VARCHAR(50) NOT NULL,  -- WRONG_SKILL, SLOW_EXECUTION, etc.
    signal_data JSON NOT NULL,
    created_at TIMESTAMP NOT NULL,
    INDEX idx_event (selection_event_id),
    INDEX idx_type (signal_type),
    INDEX idx_created (created_at),
    FOREIGN KEY (selection_event_id) REFERENCES skill_selection_events(event_id)
);

-- Learned corrections (Stage 1: SELECT)
CREATE TABLE learned_corrections (
    correction_id VARCHAR(36) PRIMARY KEY,
    query_pattern VARCHAR(255) NOT NULL,
    wrong_skills TEXT,  -- Comma-separated
    correct_skills TEXT,  -- Comma-separated
    confidence FLOAT NOT NULL,
    signal_counts JSON,  -- {"WRONG_SKILL": 10, "SLOW_EXECUTION": 5}
    created_at TIMESTAMP NOT NULL,
    updated_at TIMESTAMP NOT NULL,
    INDEX idx_pattern (query_pattern)
);

-- Regression gate results (Stage 3: LEARN)
CREATE TABLE selector_gate_results (
    gate_id VARCHAR(36) PRIMARY KEY,
    selector_version VARCHAR(50) NOT NULL,
    verdict VARCHAR(20) NOT NULL,  -- PASS, FAIL
    new_avg_score FLOAT NOT NULL,
    old_avg_score FLOAT NOT NULL,
    improvement_pct FLOAT NOT NULL,
    test_count INT NOT NULL,
    learnings_applied INT NOT NULL,
    created_at TIMESTAMP NOT NULL,
    INDEX idx_created (created_at)
);
```

## Performance Characteristics

### Selection Latency

```
Stage 1: SELECT
├─ ModernSkillSelector.select_and_execute()  ~50ms
│  ├─ Database query (skill registry)        ~10ms
│  ├─ LLM function calling                   ~30ms
│  └─ Schema generation                      ~10ms
│
└─ SelfImprovingSelector.apply_learnings()   ~20ms
   ├─ Pattern matching                       ~10ms
   └─ Correction application                 ~10ms

Stage 2: AUDIT
└─ Create selection event                    ~30ms
   ├─ Generate event_id                      ~1ms
   ├─ Create snapshot                        ~10ms
   └─ Database insert                        ~20ms

Total: ~100ms (p95)
```

### Learning Cycle

```
Stage 3: LEARN (1000 signals)
├─ Collect signals                           ~500ms
│  └─ SELECT * FROM learning_signals         ~500ms
│
├─ Multi-factor scoring                      ~1000ms
│  ├─ Group by pattern                       ~200ms
│  ├─ Calculate scores                       ~500ms
│  └─ Generate corrections                   ~300ms
│
└─ Regression gate                           ~3000ms
   ├─ Load golden queries                    ~100ms
   ├─ Test old selector (20 queries)         ~1000ms
   ├─ Test new selector (20 queries)         ~1000ms
   └─ Compare results                        ~100ms
   └─ Store gate result                      ~100ms

Total: ~5s (for 1000 signals)
```

## Next Steps

1. ✅ Create design documents
2. ⏳ Implement `UnifiedSkillSelector`
3. ⏳ Add unit tests
4. ⏳ Update ChatLoop integration
5. ⏳ Update CLI and API
6. ⏳ Run full test suite
7. ⏳ Deploy to staging
8. ⏳ Cleanup old code
