---
name: evaluate-session
description: "Platform skill: evaluates agent session performance and provides optimization recommendations. Requires astra platform database access."
user_invocable: true
when_to_use: "When the user wants to evaluate agent performance metrics or get optimization recommendations for a session"
arguments:
  - name: SESSION
    description: "Session ID or reference (e.g., 'this session', 'last session', or a specific ID)"
    required: false
  - name: OBJECTIVE
    description: "Optional optimization objective: cost, accuracy, latency, or balanced"
    required: false
allowed_tools:
  - bash
  - read_file
---
# Evaluate Session Skill

Evaluate agent session performance and provide optimization recommendations.

## Task

$ARGUMENTS

## 1. Identify the Target Session

- Specific session ID → use that
- "this session" / "current" → current session ID
- "last session" / "previous" → most recent completed session

## 2. Gather Session Data

Use `astra journal digest` first (preferred — offline, no DB needed). Fall back to SQL only when digest is unavailable or when cross-session comparison is needed.

```bash
# Preferred: use journal digest (offline, no login)
astra journal digest <session_id>
astra journal digest --focus summary
```

If digest is unavailable, query the cloud database:

```sql
SELECT 
  event_type, model, skill_name,
  prompt_tokens, completion_tokens, created_at
FROM agent_events
WHERE session_id = '<session_id>'
ORDER BY created_at;
```

## 3. Calculate Metrics

### Token Metrics
- **Total Prompt / Completion / Combined Tokens**
- **Average Tokens per LLM Call**: total / llm_call_count

### Call Metrics
- **User Queries**: Count of `user_query` events
- **LLM Calls**: Count of `llm_call` events
- **Calls per Query**: llm_calls / user_queries

### Skill Usage
- **Unique Skills**, **Total Skill Calls**, **Breakdown** per skill

## 4. Assess Efficiency

### Token Efficiency

| Rating | Tokens per Query |
|--------|------------------|
| 🟢 excellent | < 10,000 |
| 🟡 good | 10,000 - 19,999 |
| 🟠 moderate | 20,000 - 39,999 |
| 🔴 needs_improvement | ≥ 40,000 |

### Call Efficiency

| Rating | Calls per Query |
|--------|-----------------|
| 🟢 excellent | ≤ 2 |
| 🟡 good | 2.1 - 4 |
| 🟠 moderate | 4.1 - 6 |
| 🔴 needs_improvement | > 6 |

### Overall: "good" if both are excellent/good; "needs_improvement" otherwise.

## 5. Optimization (when OBJECTIVE is provided)

If the user requests optimization, calculate dimension scores and suggest actions:

| Objective | Focus | Trade-offs |
|-----------|-------|------------|
| **cost** | Reduce tokens, compress history, smaller model | May reduce context quality |
| **accuracy** | Increase context, add verification, larger model | Higher cost, slower |
| **latency** | Reduce call count, parallelize, cache results | May miss context updates |
| **balanced** | Weighted: cost 0.3, accuracy 0.4, latency 0.3 | Moderate trade-offs |

### Scoring
- **Cost**: `1 - (actual_tokens / baseline_tokens)` where baseline = user_queries × 20,000 (moderate threshold)
- **Accuracy**: `successful_completions / total_completions`
- **Latency**: `1 - (actual_calls / baseline_calls)` where baseline = user_queries × 4 (good threshold)

Show before/after comparison with explicit trade-off impact.

## 6. Report Format

```
## Session Evaluation: {session_id}

### Summary
- Total Events: {count}  |  User Queries: {count}  |  LLM Calls: {count}

### Token Usage
| Metric | Value |
|--------|-------|
| Prompt Tokens | {n} |
| Completion Tokens | {n} |
| Total Tokens | {n} |
| Avg per Call | {n} |

### Efficiency
- Token: {rating} ({n} tokens/query)
- Call: {rating} ({n} calls/query)
- Overall: {overall}

### Skill Usage
{breakdown_table}

### Optimization (if requested)
| Dimension | Before | After | Change |
|-----------|--------|-------|--------|
| Cost      | {score} | {score} | {delta} |
| Accuracy  | {score} | {score} | {delta} |
| Latency   | {score} | {score} | {delta} |

### Recommendations
{specific_recommendations}
```

## 7. Recommendations

**High token usage:** More concise prompts, smaller steps, targeted tools.
**High call count:** Batch operations, comprehensive tools, plan before executing.
**Good performance:** Note what worked well, maintain patterns.
**Cost optimization:** Aggressive compaction, reduce history, smaller model.
**Accuracy optimization:** Larger context, verification steps, larger model.
**Latency optimization:** Fewer calls, parallel tools, caching.

---

## Reference: Key Source Files

| Component | File |
|-----------|------|
| Journal digest CLI | `rust/crates/astra-cli/src/cli/journal_digest.rs` |
| Session journal | `rust/crates/services/src/session_journal.rs` |
| Event ingestion | `rust/crates/services/src/event_ingestion.rs` |
