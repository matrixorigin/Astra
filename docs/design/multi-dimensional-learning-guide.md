# Multi-Dimensional Learning - Usage Guide

## Overview

Phase 1 of the self-improving selector adds multi-dimensional learning, enabling automatic optimization across accuracy, speed, cost, and user satisfaction.

## Quick Start

### Basic Usage (Default Configuration)

```python
from core.skills.pipeline import SkillPipeline

# Create pipeline with learning enabled
pipeline = SkillPipeline(
    db=db,
    llm_client=llm_client,
    learning=True,
)

# Trigger learning from recent failures
result = pipeline.learn(days=7)

print(f"Learned {result.learned} patterns")
print(f"Signal breakdown: {result.signals_by_type}")
```

## Custom Configuration

### Configure Weights

Adjust the importance of each dimension:

```python
from core.skills.learning_signals import SignalWeights

# Cost-sensitive scenario
weights = SignalWeights(
    accuracy=0.3,      # 30% - still important
    speed=0.2,         # 20% - moderate
    cost=0.4,          # 40% - highest priority
    satisfaction=0.1   # 10% - lowest
)

pipeline = SkillPipeline(
    db=db,
    llm_client=llm_client,
    learning=True,
    learning_weights=weights,
)
```

### Configure Thresholds

Adjust when signals are triggered:

```python
from core.skills.learning_signals import SignalThresholds, SignalWeights
from core.skills.pipeline import SkillPipeline

# Stricter thresholds (configured via Config table)
# See api/models.py Config for threshold configuration

# Custom weights
weights = SignalWeights(
    accuracy=0.3,
    speed=0.2,
    cost=0.4,
    satisfaction=0.1
)

pipeline = SkillPipeline(
    db=db,
    llm_client=llm_client,
    weights=weights,
    thresholds=thresholds,
)
```

## Selective Learning

Learn from specific signal types only:

```python
from core.skills.learning_signals import SignalType

# Phase 1: Focus on accuracy
result = pipeline.learn(
    days=7,
    signal_types=[SignalType.WRONG_SKILL],
)

# Phase 2: Add performance optimization
result = pipeline.learn(
    days=7,
    signal_types=[
        SignalType.WRONG_SKILL,
        SignalType.SLOW_EXECUTION,
    ],
)

# Phase 3: Full optimization
result = pipeline.learn(
    days=7,
    signal_types=[
        SignalType.WRONG_SKILL,
        SignalType.SLOW_EXECUTION,
        SignalType.HIGH_COST,
        SignalType.LOW_SATISFACTION,
    ],
)
```

## API Usage

### Trigger Learning

```bash
curl -X POST http://localhost:8000/api/v1/learning/trigger \
  -H "Content-Type: application/json" \
  -d '{
    "days": 7,
    "signal_types": ["wrong_skill", "slow_execution", "high_cost"],
    "weights": {
      "accuracy": 0.4,
      "speed": 0.3,
      "cost": 0.2,
      "satisfaction": 0.1
    }
  }'
```

### Get Available Signal Types

```bash
curl http://localhost:8000/api/v1/learning/signals
```

Response:
```json
{
  "signal_types": [
    "wrong_skill",
    "slow_execution",
    "high_cost",
    "low_satisfaction"
  ],
  "descriptions": {
    "wrong_skill": "Incorrect skill selection",
    "slow_execution": "Execution time exceeds threshold",
    "high_cost": "Execution cost exceeds budget",
    "low_satisfaction": "User satisfaction below threshold"
  }
}
```

### Get Learning Statistics

```bash
curl http://localhost:8000/api/v1/learning/stats
```

Response:
```json
{
  "total_learnings": 25,
  "high_confidence": 15,
  "low_confidence": 10,
  "avg_confidence": 65.5,
  "by_signal_type": {
    "wrong_skill": 10,
    "slow_execution": 8,
    "high_cost": 5,
    "low_satisfaction": 2
  },
  "weights": {
    "accuracy": 0.4,
    "speed": 0.3,
    "cost": 0.2,
    "satisfaction": 0.1
  },
  "total_gates": 5,
  "passed_gates": 4,
  "failed_gates": 1,
  "pass_rate": 0.8,
  "avg_improvement_pct": 12.5
}
```

## Signal Types

### 1. Wrong Skill (accuracy)

**Trigger**: `selection_correctness = 0` and `correction_suggestion` provided

**Target**: Select correct skills

**Example**:
```python
# User query: "Create a pull request"
# Selected: ["github_list_repos"]  ❌
# Correct: ["github_create_pr"]    ✅
```

### 2. Slow Execution (speed)

**Trigger**: `execution_time_ms > threshold` (default: 5000ms)

**Target**: 50% faster execution

**Example**:
```python
# Execution time: 10 seconds
# Target: 5 seconds
```

### 3. High Cost (cost)

**Trigger**: `execution_cost > threshold` (default: $0.10)

**Target**: 50% cost reduction

**Example**:
```python
# Execution cost: $0.50
# Target: $0.25
```

### 4. Low Satisfaction (satisfaction)

**Trigger**: `user_feedback_score < threshold` (default: < 3 stars)

**Target**: 4+ stars

**Example**:
```python
# User feedback: 2 stars ⭐⭐
# Target: 4 stars ⭐⭐⭐⭐
```

## Multi-Factor Scoring

Each selection is scored across all dimensions:

```python
score = (
    accuracy_score * 0.4 +
    speed_score * 0.3 +
    cost_score * 0.2 +
    satisfaction_score * 0.1
)
```

**Score ranges**:
- 0-100 per dimension
- Weighted average for final score
- Higher is better

## Best Practices

### 1. Start Conservative

Begin with default thresholds and weights, then adjust based on your needs:

```python
# Start with defaults
pipeline = SkillPipeline(db, llm_client, enable_learning=True)

# Monitor for 1 week
result = pipeline.learn(days=7)

# Adjust if needed
weights = SignalWeights(accuracy=0.5, speed=0.2, cost=0.2, satisfaction=0.1)
```

### 2. Gradual Rollout

Enable signal types progressively:

```python
# Week 1: Accuracy only
signal_types = [SignalType.WRONG_SKILL]

# Week 2: Add performance
signal_types = [SignalType.WRONG_SKILL, SignalType.SLOW_EXECUTION]

# Week 3: Full optimization
signal_types = list(SignalType)
```

### 3. Monitor Regression Gate

Check gate pass rate to ensure learning improves quality:

```python
stats = selector.get_learning_stats()
print(f"Gate pass rate: {stats['regression_gates']['pass_rate']}")

# If pass rate < 0.8, review learnings
if stats['regression_gates']['pass_rate'] < 0.8:
    print("Warning: Low gate pass rate, review recent learnings")
```

### 4. Adjust for Your Domain

Different domains have different priorities:

**Cost-sensitive (batch processing)**:
```python
SignalWeights(accuracy=0.3, speed=0.2, cost=0.4, satisfaction=0.1)
```

**User-facing (interactive)**:
```python
SignalWeights(accuracy=0.3, speed=0.4, cost=0.1, satisfaction=0.2)
```

**Mission-critical (accuracy first)**:
```python
SignalWeights(accuracy=0.6, speed=0.2, cost=0.1, satisfaction=0.1)
```

## Troubleshooting

### No Learnings Generated

**Possible causes**:
1. No failures in the time window
2. Thresholds too strict
3. Missing correction suggestions

**Solution**:
```python
# Check recent failures
failures = selector.improving_selector.get_recent_failures(days=7)
print(f"Found {len(failures)} failures")

# Lower thresholds temporarily
thresholds = SignalThresholds(
    slow_execution_ms=3000,  # Lower from 5s
    high_cost_usd=0.05,      # Lower from $0.10
    low_satisfaction=4,      # Raise from 3
)
```

### Low Confidence Learnings

**Possible causes**:
1. Insufficient evidence (evidence_count < 3)
2. Conflicting signals

**Solution**:
```python
# Wait for more evidence
stats = selector.get_learning_stats()
print(f"Avg confidence: {stats['learnings']['avg_confidence']}")

# Only apply high-confidence learnings
learnings = db.query(SkillSelectionLearning).filter(
    SkillSelectionLearning.confidence >= 70
).all()
```

### Regression Gate Failures

**Possible causes**:
1. Learning degraded quality
2. Golden queries outdated

**Solution**:
```python
# Review failed gate
from api.models import SelectorGateResult
failed_gates = db.query(SelectorGateResult).filter(
    SelectorGateResult.verdict == "FAIL"
).order_by(SelectorGateResult.created_at.desc()).limit(5).all()

# Rollback if needed
# (Manual intervention required)
```

## See Also

- [Self-Improving Selector Architecture](self-improving-selector-architecture.md)
- [Learning Evolution Roadmap](learning-evolution-roadmap.md)
- [API Reference](../api-reference.md)
