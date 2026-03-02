# Reflect Call Chain Visualization — Implementation Status

## Implemented

Execution tree visualization for session analysis, inspired by SQL `EXPLAIN ANALYZE`.

## Core Components

### 1. ExecutionNode — Tree Data Structure

Each node represents one event (user_query, llm_response, tool_call, tool_result)
with timing, token usage, cost, and auto-detected issues.

```
session (140.00s)
  └─ user_query: "list all open PRs"
      ├─ llm_response (1.50s) [1200→85 tokens] $0.0002
      │   ├─ tool_call: list_prs
      │   │   └─ tool_result: list_prs (8.20s) ⚠️ SLOW
      │   │       ├─ api_latency: 8200ms
      │   │       └─ tokens_added: 3,200
      │   └─ tool_call: get_pr_details
      │       └─ tool_result: get_pr_details (2.10s)
      └─ llm_response (0.80s) [4500→120 tokens]  ← turn 2 after tool results
```

Multi-turn tool-use loops are fully represented: each `llm_response` in the
loop is a sibling under the same `user_query`.

### 2. ExecutionSummary — Aggregated Stats

- Time breakdown by category (leaf nodes only — no double-counting)
- Token breakdown (prompt vs completion)
- Cost breakdown by turn
- Auto-detected root causes

### 3. Issue Detection

| Tag | Condition |
|-----|-----------|
| `SLOW` | Node duration ≥ 10s |
| `BOTTLENECK` | Node > 50% of parent duration |
| `HIGH_TOKEN` | Prompt tokens > 5,000 |
| `EXPENSIVE` | Single call > $0.01 |
| `LARGE_CONTEXT` | Tool result > 2,000 tokens |

### 4. Cost Calculation

Built-in pricing table with prefix-matching fallback (e.g. `gpt-4o-2024-08-06`
matches `gpt-4o`). Logs a warning when model is not recognized.

## Tree Building Algorithm

Sequential state-machine walk over timestamp-ordered events:

1. `user_query` → new child of session root, resets state
2. `llm_response` → child of current user_query
3. `tool_call` → child of current llm_response
4. `tool_result` → matched to its tool_call by name (reverse walk)

This handles arbitrary multi-turn loops without the limitations of the
previous recursive approach (which only captured the first llm_response).

## Files Modified

- `core/agent/session_analyzer.py` — all implementation
- `SessionReport.to_markdown()` — renders tree + summary

## Not Yet Implemented

The following are planned but not yet in code:

- Phase-level sub-nodes (prompt_assembly, model_inference, memory_retrieval)
- Token breakdown by source (system_prompt vs history vs tool_results)
- Parallel execution visualization
- Flamegraph export
- Interactive HTML tree
- Session diff (historical comparison)

## Test Coverage

Existing `tests/unit/test_session_analysis.py` covers the `SessionAnalyzer`
integration path. Dedicated unit tests for the new tree-building and summary
logic are included in the same file.
