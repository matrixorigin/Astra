# Design: Reflect Skill

**Date**: 2026-03-01
**Status**: Final

## Problem

The current `reflect` is an edge tool that formats local conversation history.
This is useless — the LLM already has conversation history in its context window.
Real reflection requires server-side data the edge can't access.

## Architecture Decision: Edge Tool + Server Endpoint

Reflect cannot be a pure cloud skill because `/chat/turn` only exposes edge tools
to the LLM. Cloud skills (`ToolRegistry`) are only used in the `/chat` endpoint.

The proven pattern is **edge tool + server endpoint** — same as `GetAgentInfoTool`
which calls `/introspection/memory` and `/introspection/skills` for cloud data.

```
LLM calls reflect(focus="skill_failure")
    │
    ▼
ReflectTool (edge)
    │  HTTP GET /chat/session/{sid}/reflect?focus=skill_failure
    ▼
Server endpoint (cloud)
    │
    ├── EventReader.get_session_events()     → event trail with timing/tokens
    ├── SQL: skill_selection_events          → selection history + outcomes
    ├── MemoryStore.list_active(PROCEDURAL)  → past lessons
    ├── SQL: ctx_prompt_feedback             → implicit feedback signals
    ├── SQL: ctx_decision_audits             → decision reasoning
    └── Mechanical diagnosis_hints           → deterministic observations
    │
    ▼
JSON evidence report → edge tool → LLM analyzes
```

## What the LLM Gets (that it can't see in context window)

| Dimension | Table | Invisible Data |
|-----------|-------|----------------|
| Event trail | `agent_events` | Token usage, model_selected events, execution timing |
| Skill decisions | `skill_selection_events` | Candidate scores, selection reasoning, success rate |
| Past lessons | `mem_memories` | Procedural memories from previous sessions |
| User signals | `ctx_prompt_feedback` | Implicit frustration/correction signals |
| Decision audit | `ctx_decision_audits` | Recorded reasoning and confidence |

## Server Endpoint

```
GET /chat/session/{session_id}/reflect?focus=auto&last_n=20
Authorization: Bearer <token>

Response:
{
  "session_id": "...",
  "focus": "skill_failure",
  "event_summary": [...],
  "skill_history": [...],
  "past_lessons": [...],
  "feedback_signals": [...],
  "diagnosis_hints": [...]
}
```

### Focus Modes

| Focus | Queries | Trigger |
|-------|---------|---------|
| `skill_failure` | Failed selections + past lessons for that skill | Skill returned `success=False` |
| `unexpected_result` | Event chain + context snapshot | Tool succeeded but wrong result |
| `data_quality` | Tool results with provenance check | Suspected fabrication/stale data |
| `auto` | Scan last N events, pick most relevant focus | Default |

### `diagnosis_hints` (deterministic, not LLM)

- Repeated skill failures in session
- Missing data_source provenance on tool results
- Token budget truncation detected
- Stale data (data_timestamp > 1 hour old)
- Matching procedural memory from past sessions

## Edge Tool

Thin client — calls server endpoint, returns JSON to LLM:

```python
class ReflectTool(EdgeTool):
    name = "reflect"
    side_effect = SideEffect.READ

    async def execute(self, focus="auto", last_n=20, **_):
        data = await self._api_client.get_reflect(
            self._session["session_id"], focus=focus, last_n=last_n)
        return json.dumps(data, indent=2)
```

## Files Changed

| File | Action |
|------|--------|
| `cli/tools/reflect.py` | REWRITE — thin HTTP client |
| `cli/api_client.py` | ADD `get_reflect()` method |
| `api/routers/chat.py` | ADD `GET /chat/session/{sid}/reflect` endpoint |
| `cli/edge_chat_loop.py` | REMOVE `session_state` parameter + history population |
| `rust/crates/astra-cli` (`/reflect` path) | REMOVE `session_state`, simplify ReflectTool construction |
| `tests/unit/test_reflect.py` | REWRITE — test endpoint + edge tool + diagnosis logic |

---

## Known Limitations

Reflect is strong at structural analysis (what happened, when, which tools) but weak at semantic analysis (was the result good, was the cost reasonable). This is by design — reflect is a **data retrieval** tool, not a **quality evaluation** tool. The LLM interprets the data; reflect provides it.

| Dimension | Capability | Gap | Evolution Path |
|-----------|-----------|-----|----------------|
| Event timeline | ⭐⭐⭐⭐⭐ Precise timestamps, causal chains | — | — |
| Tool call counts | ⭐⭐⭐⭐⭐ Complete per-session and per-skill | — | — |
| Success/failure | ⭐⭐⭐⭐ `success` status from skill_selection_events | No error categorization (transient vs permanent) | Add `error_category` to skill_selection_events |
| Performance | ⭐⭐ `execution_time_ms` on some tools | Missing on edge tools; no latency breakdown (network vs compute) | Edge tools report `execution_time_ms` in tool_result metadata |
| Token usage | ⭐ Not surfaced | `token_usage` exists in `agent_events` but reflect doesn't include it | **Phase 1**: Add `token_summary` to reflect response (sum prompt/completion tokens per LLM call in session) |
| Content quality | ⭐ Cannot assess | Reflect returns event data, not quality judgments | **Addressed by**: [tool-result-quality-firewall.md](tool-result-quality-firewall.md) — quality signals are now attached to tool results before they reach the LLM. Reflect can surface these signals via `quality_grade` field in event_summary. |
| Cost analysis | ⭐ Cannot compute | No cost data in events | **Phase 2**: Add `estimated_cost` to LLM response events (model × tokens × price_per_token from model registry). Reflect surfaces cumulative session cost. |

### Phase 1: Token and Quality Visibility (Low Effort)

Extend `_build_reflect_evidence()` to include two new fields:

```python
# In reflect endpoint response, add:
{
    # ... existing fields ...

    # NEW: Token summary from agent_events
    "token_summary": {
        "total_prompt_tokens": 25010,
        "total_completion_tokens": 812,
        "llm_calls": 4,
        "avg_prompt_per_call": 6252,
    },

    # NEW: Quality grades from tool_result_quality events (if firewall enabled)
    # Allows reflect to answer "was the data any good?"
    "tool_quality_summary": [
        {"tool": "stock_assistant", "grade": "degraded", "score": 0.35,
         "signals": ["technical_indicators empty", "risk_score is default"]},
    ],
}
```

Data sources:
- `token_summary`: `SELECT SUM(token_usage->>'$.prompt_tokens'), ... FROM agent_events WHERE session_id = :sid AND event_type = 'llm_response'`
- `tool_quality_summary`: `SELECT metadata FROM agent_events WHERE session_id = :sid AND event_type = 'tool_result_quality'`

Both queries use existing indexed columns. No schema changes needed.

### Phase 2: Cost Visibility (Requires Model Registry)

Depends on model pricing data in the model registry (not yet implemented). Once available:

```python
"cost_summary": {
    "total_estimated_cost_usd": 0.038,
    "breakdown": [
        {"model": "deepseek-chat", "calls": 4, "tokens": 25822, "cost_usd": 0.038},
    ],
}
```

### Relationship to Tool Result Quality Firewall

The quality firewall (tool-result-quality-firewall.md) and reflect are complementary:

| Concern | Quality Firewall | Reflect |
|---------|-----------------|---------|
| When | Pre-LLM (before the LLM sees the result) | Post-hoc (after the turn, on demand) |
| Purpose | Annotate data so LLM responds honestly | Diagnose what happened and why |
| Content quality | Assesses tool result completeness | Surfaces quality grades from firewall events |
| Audience | The LLM (via annotation in context) | The LLM or user (via reflect tool call) |

With the quality firewall in place, reflect gains the ability to answer "was the data any good?" by reading `tool_result_quality` events — closing its biggest gap without adding quality assessment logic to reflect itself.
