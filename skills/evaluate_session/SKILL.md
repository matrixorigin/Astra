---
name: evaluate-session
description: "Agent self-assessment skill that evaluates performance metrics for a session"
user_invocable: true
triggers:
  - evaluate
  - evaluate session
  - session metrics
  - performance
  - efficiency
  - how efficient
  - analyze session
  - 评估
  - 评估会话
  - 会话性能
  - 性能分析
  - 效率
allowed_tools:
  - bash
  - read_file
---
# Evaluate Session Skill

When asked to evaluate a session's performance, follow this systematic approach:

## 1. Identify the Target Session

**Determine which session to evaluate:**
- If a specific session ID is provided, use that
- If user says "this session" or "current", use the current session ID
- If user says "last session" or "previous", look up the most recent completed session

## 2. Gather Session Data

Query the `agent_events` table for the session:

```sql
SELECT 
  event_type,
  model,
  skill_name,
  prompt_tokens,
  completion_tokens,
  created_at
FROM agent_events
WHERE session_id = '<session_id>'
ORDER BY created_at;
```

## 3. Calculate Metrics

### Token Metrics
- **Total Prompt Tokens**: Sum of all prompt_tokens
- **Total Completion Tokens**: Sum of all completion_tokens
- **Total Tokens**: prompt + completion
- **Average Tokens per LLM Call**: total / llm_call_count

### Call Metrics
- **User Queries**: Count of `user_query` events
- **LLM Calls**: Count of `llm_call` events
- **Calls per Query**: llm_calls / user_queries

### Skill Usage
- **Unique Skills**: Count distinct skill names
- **Total Skill Calls**: Count of events with skill_name
- **Breakdown**: Count per skill

## 4. Assess Efficiency

### Token Efficiency Rating

| Rating | Tokens per Query |
|--------|------------------|
| 🟢 excellent | < 10,000 |
| 🟡 good | 10,000 - 19,999 |
| 🟠 moderate | 20,000 - 39,999 |
| 🔴 needs_improvement | ≥ 40,000 |

### Call Efficiency Rating

| Rating | Calls per Query |
|--------|-----------------|
| 🟢 excellent | ≤ 2 |
| 🟡 good | 2.1 - 4 |
| 🟠 moderate | 4.1 - 6 |
| 🔴 needs_improvement | > 6 |

### Overall Assessment
- **good**: Both token and call efficiency are excellent or good
- **needs_improvement**: Either metric is moderate or worse

## 5. Report Format

Present the evaluation in this format:

```
## Session Evaluation: {session_id}

### Summary
- **Total Events**: {count}
- **User Queries**: {count}
- **LLM Calls**: {count}

### Token Usage
| Metric | Value |
|--------|-------|
| Prompt Tokens | {prompt_tokens} |
| Completion Tokens | {completion_tokens} |
| Total Tokens | {total} |
| Avg per Call | {avg} |

### Efficiency Assessment
- **Token Efficiency**: {rating} ({tokens_per_query} tokens/query)
- **Call Efficiency**: {rating} ({calls_per_query} calls/query)
- **Overall**: {overall}

### Skill Usage
{skill_breakdown_table}

### Recommendations
{specific_recommendations_based_on_metrics}
```

## 6. Provide Recommendations

Based on the assessment, suggest improvements:

**For high token usage:**
- Consider more concise prompts
- Break complex tasks into smaller steps
- Use more targeted tools instead of broad searches

**For high call count:**
- Batch related operations
- Use more comprehensive tools
- Plan before executing

**For good performance:**
- Note what worked well
- Suggest maintaining current patterns

---

**Note**: This skill enables agent self-awareness and continuous improvement tracking.
