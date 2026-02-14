# Self-Improving Selector Architecture

## System Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│                         AgentSkillSelector                           │
│                    (Main Entry Point)                                │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────┐    │
│  │  select_skills(query, context)                              │    │
│  │                                                              │    │
│  │  1. Get candidates from AuditableSkillSelector              │    │
│  │  2. Apply learned corrections (if enabled)                  │    │
│  │  3. Return tool calls                                       │    │
│  └────────────────────────────────────────────────────────────┘    │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────┐    │
│  │  learn_from_failures(days)                                  │    │
│  │                                                              │    │
│  │  1. Trigger SelfImprovingSelector.learn_from_failures()    │    │
│  │  2. Validate through RegressionGate                         │    │
│  │  3. Record results to database                              │    │
│  │  4. Return learning statistics                              │    │
│  └────────────────────────────────────────────────────────────┘    │
└───────────────────────────┬───────────────────────────────────────┘
                            │
        ┌───────────────────┼───────────────────┐
        │                   │                   │
        ▼                   ▼                   ▼
┌──────────────┐  ┌──────────────────┐  ┌──────────────┐
│  Auditable   │  │  SelfImproving   │  │  Regression  │
│  Selector    │  │  Selector        │  │  Gate        │
└──────────────┘  └──────────────────┘  └──────────────┘
```

## The Closed Loop Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                    PRODUCTION USAGE                              │
│                                                                   │
│  User Query: "Create a GitHub PR"                                │
│       ↓                                                           │
│  AgentSkillSelector.select_skills()                              │
│       ↓                                                           │
│  ┌─────────────────────────────────────────────────┐            │
│  │ 1. AuditableSkillSelector                        │            │
│  │    - Get candidates: [github_create_pr, ...]    │            │
│  │    - Record selection event                      │            │
│  └─────────────────────────────────────────────────┘            │
│       ↓                                                           │
│  ┌─────────────────────────────────────────────────┐            │
│  │ 2. SelfImprovingSelector.apply_learnings()      │            │
│  │    - Check learned patterns                      │            │
│  │    - Filter wrong skills                         │            │
│  │    - Suggest correct skills                      │            │
│  └─────────────────────────────────────────────────┘            │
│       ↓                                                           │
│  Return: [github_create_pr]                                      │
│                                                                   │
│  ┌─────────────────────────────────────────────────┐            │
│  │ 3. Execution & Feedback                          │            │
│  │    - Execute skill                               │            │
│  │    - Record result (success/failure)             │            │
│  │    - User feedback (1-5 stars)                   │            │
│  │    - Update selection_correctness                │            │
│  └─────────────────────────────────────────────────┘            │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                    LEARNING CYCLE                                │
│                (Triggered manually or scheduled)                 │
│                                                                   │
│  Trigger: mo-agent skill learn --days 7                          │
│       ↓                                                           │
│  AgentSkillSelector.learn_from_failures(7)                       │
│       ↓                                                           │
│  ┌─────────────────────────────────────────────────┐            │
│  │ 1. OBSERVE: Get recent failures                 │            │
│  │    SELECT * FROM skill_selection_events         │            │
│  │    WHERE selection_correctness = 0              │            │
│  │    AND created_at >= NOW() - 7 days             │            │
│  │                                                  │            │
│  │    Found: 5 failures                            │            │
│  └─────────────────────────────────────────────────┘            │
│       ↓                                                           │
│  ┌─────────────────────────────────────────────────┐            │
│  │ 2. DIAGNOSE: Extract patterns                   │            │
│  │    For each failure:                             │            │
│  │      - query_pattern: "create pr"               │            │
│  │      - wrong_skills: ["wrong_tool"]             │            │
│  │      - correct_skills: ["github_create_pr"]     │            │
│  │      - improvement_score: 10                    │            │
│  │                                                  │            │
│  │    Learned: 3 new patterns                      │            │
│  └─────────────────────────────────────────────────┘            │
│       ↓                                                           │
│  ┌─────────────────────────────────────────────────┐            │
│  │ 3. VALIDATE: Regression gate                    │            │
│  │    Get golden queries (20 high-quality)         │            │
│  │    Test old selector: avg_score = 0.85          │            │
│  │    Test new selector: avg_score = 0.90          │            │
│  │    Improvement: +5.9%                            │            │
│  │                                                  │            │
│  │    Verdict: PASS ✅                              │            │
│  └─────────────────────────────────────────────────┘            │
│       ↓                                                           │
│  ┌─────────────────────────────────────────────────┐            │
│  │ 4. DEPLOY: Activate learnings                   │            │
│  │    UPDATE skill_selection_learning              │            │
│  │    SET confidence = 30 (3 evidence)             │            │
│  │    WHERE learning_id = ...                      │            │
│  │                                                  │            │
│  │    INSERT INTO selector_gate_results            │            │
│  │    (verdict='PASS', improvement_pct=5.9, ...)   │            │
│  └─────────────────────────────────────────────────┘            │
│       ↓                                                           │
│  Return: {learned: 3, gate_verdict: 'pass', improvement: 5.9}   │
└─────────────────────────────────────────────────────────────────┘
```

## Database Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                    DATABASE TABLES                               │
│                                                                   │
│  ┌──────────────────────────────────────────────────┐           │
│  │  skill_selection_events                           │           │
│  │  ─────────────────────────────────────────────   │           │
│  │  Every selection recorded with:                   │           │
│  │  - Query, candidates, scores                      │           │
│  │  - Execution result                               │           │
│  │  - User feedback                                  │           │
│  │  - Correctness flag                               │           │
│  └──────────────────────────────────────────────────┘           │
│                    ↓                                              │
│  ┌──────────────────────────────────────────────────┐           │
│  │  skill_selection_learning                         │           │
│  │  ─────────────────────────────────────────────   │           │
│  │  Learned patterns:                                │           │
│  │  - Query pattern                                  │           │
│  │  - Wrong skills → Correct skills                  │           │
│  │  - Confidence (increases with evidence)           │           │
│  │  - Applied count (tracks usage)                   │           │
│  └──────────────────────────────────────────────────┘           │
│                    ↓                                              │
│  ┌──────────────────────────────────────────────────┐           │
│  │  selector_gate_results                            │           │
│  │  ─────────────────────────────────────────────   │           │
│  │  Gate validations:                                │           │
│  │  - Verdict (PASS/FAIL)                            │           │
│  │  - Old vs new scores                              │           │
│  │  - Improvement percentage                         │           │
│  │  - Learnings applied count                        │           │
│  └──────────────────────────────────────────────────┘           │
└─────────────────────────────────────────────────────────────────┘
```

## Confidence Evolution

```
Learning Lifecycle:

Initial observation:
  confidence = 10 (1 evidence)
  applied = NO (confidence < 50)

After 3 more observations:
  confidence = 40 (4 evidence)
  applied = NO (still < 50)

After 5 observations:
  confidence = 50 (5 evidence)
  applied = YES ✅ (confidence >= 50)

After 10 observations:
  confidence = 99 (10 evidence, capped at 99)
  applied = YES
  applied_count = 25 (used 25 times)
```

## Safety Mechanisms

```
┌─────────────────────────────────────────────────────────────────┐
│                    SAFETY GUARANTEES                             │
│                                                                   │
│  1. Regression Gate                                              │
│     ├─ Tests on golden queries before deployment                │
│     ├─ Requires improvement >= threshold                         │
│     └─ Records verdict in database                               │
│                                                                   │
│  2. Confidence Threshold                                         │
│     ├─ Only applies learnings with confidence >= 50             │
│     ├─ Confidence increases with evidence                        │
│     └─ Max confidence capped at 99                               │
│                                                                   │
│  3. Full Audit Trail                                             │
│     ├─ Every selection recorded                                  │
│     ├─ Every learning tracked                                    │
│     └─ Every gate validation logged                              │
│                                                                   │
│  4. Reversibility                                                │
│     ├─ Can disable learning per selector                         │
│     ├─ Can reset learnings in database                           │
│     └─ No breaking changes to existing code                      │
│                                                                   │
│  5. Observable                                                   │
│     ├─ CLI commands for monitoring                               │
│     ├─ Database queries for analysis                             │
│     └─ Statistics API for dashboards                             │
└─────────────────────────────────────────────────────────────────┘
```

## Performance Characteristics

```
Operation                    Latency      Storage
─────────────────────────────────────────────────
select_skills()              ~10ms        1KB/event
  ├─ Auditable selection     ~8ms         
  └─ Apply learnings         ~2ms         

learn_from_failures()        1-5s         500B/learning
  ├─ Get failures            ~100ms       
  ├─ Extract patterns        ~500ms       
  ├─ Regression gate         ~3s          
  └─ Record results          ~50ms        

Database growth:
  ├─ Events: ~1KB × selections/day
  ├─ Learnings: ~500B × patterns
  └─ Gates: ~1KB × learning cycles
```

## Monitoring Queries

```sql
-- Learning effectiveness
SELECT 
    l.query_pattern,
    l.confidence,
    l.evidence_count,
    l.applied_count,
    l.applied_count / l.evidence_count as application_rate
FROM skill_selection_learning l
WHERE l.confidence >= 50
ORDER BY l.applied_count DESC
LIMIT 10;

-- Gate success rate
SELECT 
    DATE(created_at) as date,
    COUNT(*) as total_gates,
    SUM(CASE WHEN verdict = 'PASS' THEN 1 ELSE 0 END) as passed,
    AVG(improvement_pct) as avg_improvement
FROM selector_gate_results
GROUP BY DATE(created_at)
ORDER BY date DESC;

-- Recent failures needing attention
SELECT 
    user_query,
    selected_skills,
    correction_suggestion,
    created_at
FROM skill_selection_events
WHERE selection_correctness = 0
  AND created_at >= NOW() - INTERVAL 1 DAY
ORDER BY created_at DESC;
```
