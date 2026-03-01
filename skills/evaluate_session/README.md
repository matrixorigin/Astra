# Evaluate Session Skill

Agent self-assessment skill that evaluates performance metrics for a session.

## Overview

This skill enables the agent to analyze its own performance by examining:
- Token efficiency (tokens per query)
- LLM call efficiency (calls per query)
- Skill usage patterns
- Overall effectiveness

This is the skill version of the `evaluate_session.py` script, integrated into the agent's toolset.

## Usage

### As a Tool Call

The agent can call this skill directly:

```python
{
  "tool": "evaluate_session",
  "arguments": {
    "session_id": "019ca9f1-3dc6-72b3-9813-1f38f7349c53",
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

**Token Efficiency:**
- `excellent`: < 10K tokens/query
- `good`: 10K-20K tokens/query
- `moderate`: 20K-40K tokens/query
- `needs_improvement`: > 40K tokens/query

**Call Efficiency:**
- `excellent`: ≤ 2 calls/query
- `good`: 3-4 calls/query
- `moderate`: 5-6 calls/query
- `needs_improvement`: > 6 calls/query

## Benefits

1. **Self-Awareness**: Agent can monitor its own performance
2. **Real-time Feedback**: Get metrics during conversation
3. **Debugging**: Identify inefficient patterns
4. **Optimization**: Track improvements over time

## Comparison with evaluate_session.py

| Feature | Script | Skill |
|---------|--------|-------|
| Access | External command | Agent tool call |
| Timing | Post-conversation | Real-time |
| Integration | Manual | Automatic |
| Context | None | Full agent context |

## Example Conversation

```
User: "上海沪工建议买吗？"
Agent: [Uses stock_assistant, provides analysis]

User: "How efficient was that query?"
Agent: [Uses evaluate_session skill]
"That query used 9,568 tokens across 2 LLM calls, which is 'good' efficiency. 
The token usage was reasonable for a stock analysis with tool calls."
```

## Implementation Notes

- Reads from `agent_events` table
- Calculates metrics in real-time
- No external dependencies
- Works with any session in the database
