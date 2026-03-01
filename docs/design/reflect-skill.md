# Design: Reflect Skill

**Date**: 2026-03-01
**Status**: Final

## Problem

The current `reflect` is an edge tool that formats local conversation history.
This is useless — the LLM already has conversation history in its context window.
Real reflection requires server-side data the edge can't access.

## Architecture Decision: Edge Tool + Server Endpoint

Reflect cannot be a pure cloud skill because `/chat/turn` only exposes edge tools
to the LLM. Cloud skills (`SkillPipeline`) are only used in the `/chat` endpoint.

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
| `cli/mo_agent_api.py` | REMOVE `session_state`, simplify ReflectTool construction |
| `tests/unit/test_reflect.py` | REWRITE — test endpoint + edge tool + diagnosis logic |
