---
name: tune-performance
description: "Multi-objective performance optimization skill. Use when user wants to improve agent performance, reduce costs, increase accuracy, decrease latency, or optimize for a balanced approach. Supports automated tuning with trade-off analysis."
user_invocable: true
allowed_tools:
  - bash
  - read_file
  - write_file
---
# Tune Performance Skill - Multi-Objective Optimization

Automated performance tuning with support for multiple optimization objectives.

## When to Use This Skill

Activate this skill when the user:
- Asks to "optimize performance", "improve efficiency", or "tune the agent"
- Wants to reduce costs, token usage, or API expenses
- Needs better accuracy or response quality
- Complains about slow responses or high latency
- Asks for a "balanced" optimization across multiple dimensions
- Mentions specific targets like "reduce tokens by 30%"

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

## Usage Process

### Step 1: Identify the Optimization Goal

Based on user's request, determine the objective:
- "reduce costs" / "save money" / "fewer tokens" → `cost`
- "better accuracy" / "more correct" / "higher quality" → `accuracy`
- "faster" / "speed up" / "reduce latency" → `latency`
- "optimize" / "improve overall" / "balanced" → `balanced`

### Step 2: Gather Current Metrics

Query the performance baseline:
```sql
SELECT 
  COUNT(*) as total_calls,
  SUM(prompt_tokens) as total_prompt,
  SUM(completion_tokens) as total_completion,
  AVG(prompt_tokens + completion_tokens) as avg_tokens_per_call
FROM agent_events
WHERE session_id = '<session_id>'
  AND event_type = 'llm_call';
```

### Step 3: Calculate Dimension Scores

**Cost Score:**
```
cost_score = 1 - (actual_tokens / max_expected_tokens)
```

**Accuracy Score:**
```
accuracy_score = successful_completions / total_completions
```

**Latency Score:**
```
latency_score = 1 - (actual_time / max_expected_time)
```

### Step 4: Apply Optimizations

Based on the objective and current scores, apply relevant optimizations:

| Objective | Actions |
|-----------|---------|
| cost | Reduce context window, compress history, use smaller model |
| accuracy | Increase context, add verification, use larger model |
| latency | Reduce call count, enable caching, parallelize tools |
| balanced | Weighted combination of above |

### Step 5: Report Results

Present trade-off analysis:

```
## Performance Tuning Results

### Objective: {objective}

### Before/After Comparison
| Dimension | Before | After | Change |
|-----------|--------|-------|--------|
| Cost Score | 0.60 | 0.85 | ✅ +42% |
| Accuracy | 0.75 | 0.70 | ⚠️ -7% |
| Latency | 0.70 | 0.68 | ⚠️ -3% |

### Applied Optimizations
1. Enabled aggressive context compaction at 40%
2. Reduced max history to 3 turns

### Trade-offs Noted
- Accuracy may decrease for queries requiring extensive history
- Consider switching back if quality issues arise

### Recommendations
💡 Consider: Switch to smaller model for simple queries
```

---

## Example Interactions

### Example 1: Cost Optimization
```
User: "Optimize my performance for cost"
Agent: [Uses tune-performance skill]

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
Agent: [Uses tune-performance skill]

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
Agent: [Uses tune-performance skill with weights={accuracy: 0.5, cost: 0.25, latency: 0.25}]

Result:
✅ Accuracy: improved by 12%
✅ Cost: improved by 8%
⚠️ Latency: degraded by 3%
✅ Overall improvement: 15%
```

---

## Output Structure

When completing a tuning operation, provide structured output:

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
  "trade_offs": {
    "cost": {"change": 0.25, "direction": "improved"},
    "accuracy": {"change": -0.05, "direction": "degraded"}
  },
  "recommendations": [
    "✅ Cost: improved by 42%",
    "⚠️ Accuracy: degraded by 7% (trade-off)",
    "💡 Consider: Switch to smaller model for simple queries"
  ]
}
```

---

## Important Notes

- Always show trade-offs explicitly
- Never apply optimizations without showing expected impact
- Provide rollback instructions if changes degrade performance
- Consider user's stated priorities when choosing optimization strategy
