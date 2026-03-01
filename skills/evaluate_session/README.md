# Evaluate Session Skill

Agent self-assessment skill that evaluates performance metrics for a session.

## Overview

This skill enables the agent to analyze its own performance by examining:
- Token efficiency (tokens per query)
- LLM call efficiency (calls per query)
- Skill usage patterns
- Overall effectiveness

## Usage

### As a Tool Call

```python
{
  "tool": "evaluate_session",
  "arguments": {
    "target_session_id": "019ca9f1-3dc6-72b3-9813-1f38f7349c53",
    "include_details": false
  }
}
```

### Example Queries

**Simple evaluation:**
```
"Evaluate the performance of session 019ca9f1-3dc6-72b3-9813-1f38f7349c53"
```

**With details:**
```
"Give me a detailed evaluation of this session including event breakdown"
```

**Self-evaluation:**
```
"How efficient was I in this conversation?"
```

## Output

### Basic Metrics

```json
{
  "session_id": "019ca9f1-3dc6-72b3-9813-1f38f7349c53",
  "total_events": 19,
  "user_queries": 3,
  "llm_calls": 7,
  "tokens": {
    "prompt": 40827,
    "completion": 1705,
    "total": 42532,
    "avg_per_call": 6076
  },
  "skills": {
    "unique": 3,
    "total_calls": 5,
    "breakdown": {
      "stock_assistant": 2,
      "get_agent_info": 1,
      "reflect": 2
    }
  }
}
```

### Assessment

```json
{
  "assessment": {
    "token_efficiency": "moderate",
    "tokens_per_query": 14177,
    "call_efficiency": "moderate",
    "calls_per_query": 2.3,
    "overall": "needs_improvement"
  }
}
```

### Efficiency Ratings

**Token Efficiency** (tokens per query):
| Rating | Threshold |
|--------|-----------|
| excellent | < 10,000 |
| good | 10,000 - 19,999 |
| moderate | 20,000 - 39,999 |
| needs_improvement | ≥ 40,000 |

**Call Efficiency** (LLM calls per query):
| Rating | Threshold |
|--------|-----------|
| excellent | ≤ 2 |
| good | 2.1 - 4 |
| moderate | 4.1 - 6 |
| needs_improvement | > 6 |

**Overall**: "good" only if both token and call efficiency are "excellent" or "good".

## Benefits

1. **Self-Awareness**: Agent can monitor its own performance
2. **Real-time Feedback**: Get metrics during conversation
3. **Debugging**: Identify inefficient patterns
4. **Optimization**: Track improvements over time

## Implementation Notes

- Reads from `agent_events` table
- Supports database session injection for testing
- Uses `target_session_id` parameter (not `session_id`) to avoid collision with framework-injected fields
