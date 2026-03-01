# Tune Performance Skill - Multi-Objective Optimization

Automated performance tuning with support for multiple optimization objectives.

## Optimization Objectives

### 1. **Cost** - Minimize Token Usage
```python
{
  "objective": "cost",
  "target_score": 0.9  # 90% cost efficiency
}
```

**Optimizes for:**
- Reduce token usage per query
- Minimize API costs
- Aggressive context compaction

**Trade-offs:**
- May reduce context quality slightly
- Might miss some historical information

---

### 2. **Accuracy** - Maximize Quality
```python
{
  "objective": "accuracy",
  "target_score": 0.95  # 95% accuracy
}
```

**Optimizes for:**
- Response correctness
- Context completeness
- Verification steps

**Trade-offs:**
- Higher token usage (+30%)
- Slower response time
- Increased costs

---

### 3. **Latency** - Minimize Response Time
```python
{
  "objective": "latency",
  "target_score": 0.9  # 90% latency efficiency
}
```

**Optimizes for:**
- Reduce LLM call count
- Parallel tool execution
- Result caching

**Trade-offs:**
- May miss context updates
- Slight accuracy reduction

---

### 4. **Balanced** - Multi-Dimensional Scoring
```python
{
  "objective": "balanced",
  "target_score": 0.8,
  "weights": {
    "cost": 0.3,
    "accuracy": 0.4,
    "latency": 0.3
  }
}
```

**Optimizes for:**
- Weighted combination of all factors
- Customizable priorities
- Holistic improvement

---

## Usage Examples

### Example 1: Cost Optimization
```
User: "Optimize my performance for cost"
Agent: [Calls tune_performance with objective=cost]

Result:
✅ Cost: improved by 35% (0.60 → 0.81)
⚠️ Accuracy: degraded by 5% (trade-off)
✅ Overall improvement: 28%

Recommendations:
- Applied: Enable aggressive context compaction at 40%
- Applied: Reduce max history to 3 turns
💡 Consider: Switch to smaller model for simple queries
```

### Example 2: Accuracy Optimization
```
User: "I need better accuracy, cost doesn't matter"
Agent: [Calls tune_performance with objective=accuracy]

Result:
✅ Accuracy: improved by 18% (0.70 → 0.83)
⚠️ Cost: degraded by 30% (trade-off)
⚠️ Latency: degraded by 10% (trade-off)

Recommendations:
- Applied: Use larger model (gpt-4)
- Applied: Increase context window to 32K
💡 Consider: Add verification step with firewall
```

### Example 3: Balanced with Custom Weights
```
User: "Optimize for balanced performance, prioritize accuracy"
Agent: [Calls tune_performance with objective=balanced, weights={accuracy: 0.5, cost: 0.25, latency: 0.25}]

Result:
✅ Accuracy: improved by 12%
✅ Cost: improved by 8%
⚠️ Latency: degraded by 3%
✅ Overall improvement: 15%
```

---

## Background Job Integration

### Periodic Tuning
```python
# Cron job: Daily at 2 AM
{
  "schedule": "0 2 * * *",
  "skill": "tune_performance",
  "params": {
    "objective": "balanced",
    "target_score": 0.8
  },
  "trigger": {
    "type": "all_sessions",
    "filter": "last_24h"
  }
}
```

### Threshold-Based Tuning
```python
# Trigger when efficiency drops
{
  "trigger": {
    "type": "threshold",
    "condition": "cost_score < 0.6 OR latency_score < 0.7"
  },
  "skill": "tune_performance",
  "params": {
    "objective": "balanced"
  }
}
```

---

## Output Structure

```json
{
  "success": true,
  "iterations": 2,
  "objective": "cost",
  "initial_scores": {
    "cost": 0.60,
    "accuracy": 0.75,
    "latency": 0.70
  },
  "final_scores": {
    "cost": 0.85,
    "accuracy": 0.70,
    "latency": 0.68
  },
  "overall_improvement": 28.5,
  "improvements": [
    {
      "iteration": 1,
      "bottleneck": "cost",
      "adjustment": {
        "type": "reduce_tokens",
        "action": "Enable aggressive context compaction",
        "expected": "+25% cost reduction"
      },
      "trade_offs": {
        "accuracy": -0.05
      }
    }
  ],
  "trade_offs": {
    "cost": {
      "change": 0.25,
      "direction": "improved",
      "percentage": 41.7
    },
    "accuracy": {
      "change": -0.05,
      "direction": "degraded",
      "percentage": -6.7
    }
  },
  "recommendations": [
    "✅ Cost: improved by 42%",
    "⚠️ Accuracy: degraded by 7% (trade-off)",
    "💡 Consider: Switch to smaller model for simple queries"
  ]
}
```

---

## Closed-Loop Architecture

```
┌──────────────────────────────────────────────┐
│         Background Monitoring                │
│  (Continuous evaluation of all sessions)     │
└────────────────┬─────────────────────────────┘
                 │
                 ▼
         Efficiency < Threshold?
                 │
                 ├─ Yes ──────────────────────┐
                 │                             │
                 │                             ▼
                 │                    ┌────────────────┐
                 │                    │ tune_performance│
                 │                    └────────┬───────┘
                 │                             │
                 │                             ▼
                 │                    ┌────────────────┐
                 │                    │ Apply Changes  │
                 │                    └────────┬───────┘
                 │                             │
                 │                             ▼
                 │                    ┌────────────────┐
                 │                    │ Re-evaluate    │
                 │                    └────────┬───────┘
                 │                             │
                 │◄────────────────────────────┘
                 │
                 └─ No ──> Continue Monitoring
```

---

## Integration with Design

This implements the **Self-Improving** capability from the design docs:

- ✅ **Multi-Dimensional Learning** - Cost, accuracy, latency, satisfaction
- ✅ **Multi-Factor Scoring** - Weighted scoring across dimensions
- ✅ **Regression Gate** - Validate improvements before deployment
- ✅ **Closed-Loop** - Continuous monitoring and optimization
- ✅ **Background Jobs** - Automated tuning without manual intervention

---

## Real-World Scenarios

**Scenario 1: Production Cost Spike**
```
Alert: Token usage increased 50% in last hour
→ Auto-trigger tune_performance(objective="cost")
→ Apply optimizations
→ Verify cost reduction
→ Alert resolved
```

**Scenario 2: Quality Degradation**
```
Alert: Accuracy score dropped below 0.7
→ Auto-trigger tune_performance(objective="accuracy")
→ Switch to larger model
→ Add verification steps
→ Quality restored (with cost trade-off noted)
```

**Scenario 3: User Complaint about Speed**
```
User: "Responses are too slow"
→ Manual trigger: tune_performance(objective="latency")
→ Enable parallel execution
→ Add caching
→ Response time improved by 40%
```
