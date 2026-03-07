# Intent Unification: Three Systems → One RoutingDecision

> **Status**: Implemented  
> **Created**: 2026-03-06  
> **Updated**: 2026-03-07 — addressed fulltext scoring, feedback nuance, topic_shift interaction, Tier 0 maintainability, conflict resolution  
> **Related**: [token-efficient-llm-routing.md](token-efficient-llm-routing.md), [context-window-management.md](context-window-management.md), [skills-and-tools.md](skills-and-tools.md)

## Problem

The codebase has three disconnected intent classification systems that independently analyze the same user query, producing three separate results that never see each other:

| # | System | File | Output | Controls |
|---|--------|------|--------|----------|
| 1 | `classify_intent()` | `core/skills/intent_router.py` | `IntentClassification` (DEFAULT / EXTERNAL_FETCH / CONVERSATIONAL) | Tool filtering, max tool rounds |
| 2 | `IntentRouter.route()` | `core/context/intent_routing.py` | `RoutingDecision` (preference / command / feedback / question) | Context loading strategy (which prompt sections to build) |
| 3 | `classify_task()` + topic shift | `core/context/manager.py` | `TaskType` (CODE_REVIEW / DEBUGGING / PLANNING / GENERAL) + `topic_shift_score` | Token budget ratios, relevance scoring weights |

### What's wrong

1. **Redundant classification**: Three keyword-matching passes over the same query. System 1 checks "search online" / "hello". System 2 checks "remember" / "run" / "wrong". System 3 checks "review" / "debug" / "plan". None shares results.

2. **Inconsistent call sites**: System 2 runs in `api/routers/chat.py` (the `/chat/turn` endpoint) and passes `routing_decision` to `PromptAssembler.assemble()`. System 1 runs in `chat_loop.py` (the streaming loop) *after* tool selection. System 3 runs in `manager.py` *during* `build_context()`. These are two separate code paths (`/chat/turn` vs `chat_loop.stream_chat`) that don't share routing results.

3. **No unified event**: `chat_loop.py` logs `stream_intent_classification` for System 1. `api/routers/chat.py` logs a `routing_decision` event for System 2. System 3 produces no auditable event. The two events use different schemas and are not cross-referenced.

4. **Conflicting taxonomies**: "command" in System 2 (run/execute/delete) overlaps with DEFAULT in System 1. "CONVERSATIONAL" in System 1 (hello/thanks) has no equivalent in System 2. `TaskType.DEBUGGING` in System 3 has no mapping to either.

5. **Two entry points, two routing paths**: `/chat/turn` (cloud) uses `IntentRouter.route()` → `PromptAssembler`. `chat_loop.stream_chat()` (edge) uses `classify_intent()` + `ContextManager.build_context()`. The unification must handle both paths.

---

## Design: Extend RoutingDecision as the Single Source of Truth

System 2 (`RoutingDecision`) already has the richest infrastructure: Tier 0/1 cascade, adaptive threshold, async parallel execution, pluggable strategy registry. The right move is to absorb Systems 1 and 3 into it — not add a fourth abstraction.

### Unified RoutingDecision

```python
@dataclass
class RoutingDecision:
    """Single source of truth for all intent-derived decisions.

    All fields are guaranteed to be populated after IntentRouter.route() —
    callers never need None-checks or fallback defaults.
    """
    # --- Existing (System 2) ---
    plan: ContextLoadingPlan          # What to load (tools/history/memory)
    routing_result: RoutingResult     # Intent + confidence + tier
    tier1_result: Tier1Result | None = None
    threshold_used: float = 0.85

    # --- Absorbed from System 1 (tool filtering) ---
    tool_filter: ToolFilter = ToolFilter.NONE
    max_tool_rounds: int = MAX_TOOL_ROUNDS

    # --- Absorbed from System 3 (budget/scoring) ---
    task_type: TaskType = TaskType.GENERAL
    topic_shift_score: float = 0.0


class ToolFilter(str, Enum):
    NONE = "none"                # DEFAULT — no filtering
    LOCAL_BLOCKED = "local_blocked"  # EXTERNAL_FETCH — block local tools
    ALL_BLOCKED = "all_blocked"      # CONVERSATIONAL — block all tools
```

### Mapping Table

How existing intents map into the unified decision:

| System 2 intent | System 1 equivalent | System 3 equivalent | tool_filter | max_rounds | Notes |
|-----------------|---------------------|---------------------|-------------|------------|-------|
| preference | *(no equivalent)* | GENERAL | ALL_BLOCKED | 0 | "remember I use vim" — no tools needed, just store preference |
| command | DEFAULT | auto-detect | NONE | MAX | "run pytest" — needs tools |
| feedback | DEFAULT | auto-detect | NONE | MAX | "不对" — may need tools to re-execute |
| question | DEFAULT | auto-detect | NONE | MAX | General questions |
| *(no equivalent)* | CONVERSATIONAL | GENERAL | ALL_BLOCKED | 0 | "hello", "thanks" — greetings/chitchat, distinct from preference |
| *(any)* + EXTERNAL_FETCH keywords | EXTERNAL_FETCH | GENERAL | LOCAL_BLOCKED | 3 | "search online" — block local tools |

Note: System 1's CONVERSATIONAL (greetings: "hello", "thanks", "bye") and System 2's "preference" ("remember", "I prefer") are **not** the same. CONVERSATIONAL is pure chitchat with no state change; preference triggers a memory write. In the unified model, both get `tool_filter=ALL_BLOCKED` but for different reasons — CONVERSATIONAL needs no tools at all, preference only needs the memory-write side-effect (handled outside the tool loop).

`task_type` is orthogonal to intent — a "question" can be CODE_REVIEW or DEBUGGING. The existing `classify_task()` keyword matching moves into Tier 0 alongside the other keyword engines.

### Execution Order (Before → After)

**Before** — two separate code paths, three independent systems:

```
/chat/turn (cloud path — api/routers/chat.py):
  user_query
    ├─ IntentRouter.route(query)                    → RoutingDecision  (System 2)
    ├─ PromptAssembler.assemble(routing_decision=...) → sections       (uses System 2)
    └─ classify_intent() is NOT called here

chat_loop.stream_chat (edge path — core/agent/chat_loop.py):
  user_query
    ├─ context_manager.build_context(query)         → Context          (System 3: classify_task + topic_shift)
    ├─ classify_intent(user_input)                  → IntentClassification (System 1)
    └─ IntentRouter is NOT called here
```

System 2 and System 1 never run in the same path. System 3 only runs in the edge path.

**After** (single pass in both paths):
```
user_query
  └─ IntentRouter.route(query)  → RoutingDecision (unified)
       ├─ .plan                 → PromptAssembler (which sections to build)
       ├─ .tool_filter          → chat_loop / chat.py (which tools to keep)
       ├─ .max_tool_rounds      → chat_loop / chat.py (round limit)
       ├─ .task_type            → ContextManager (budget ratios, scoring weights)
       └─ .topic_shift_score    → Context (stale-context feedback)
```

---

## Implementation Plan

### Phase 0: Fix fulltext scoring in MemoryRetriever (independent)

> **This phase is an independent optimization. It can be merged as a standalone PR at any time, without waiting for or blocking any subsequent phase.**

Replace binary `keyword_matched: bool` with continuous `keyword_score: float` from `MATCH(...) AGAINST(...) AS score` in SELECT. Remove the incorrect "MO Fulltext Limitation" comment. This is a standalone improvement that doesn't depend on intent unification.

**Files**: `core/memory/retriever.py`, `core/memory/explain.py`

### Phase 1: Add fields to RoutingDecision (non-breaking)

Add `tool_filter`, `max_tool_rounds`, `task_type`, `topic_shift_score` to `RoutingDecision` with defaults that preserve current behavior. No callers change.

**Files**: `core/context/intent_routing.py`

### Phase 2: Populate new fields in Tier 0

Refactor `Tier0Engine` to use separate `KeywordRegistry` instances for each dimension (tool_filter, intent, task_type). Move `classify_task()` keywords and `classify_intent()` keywords into their respective registries. Each registry is independently testable.

**Files**: `core/context/intent_routing.py`, `core/skills/intent_router.py` (extract keyword sets)

### Phase 3: Wire RoutingDecision through both code paths

**Cloud path** (`api/routers/chat.py`): Already calls `IntentRouter.route()` and passes `routing_decision` to `PromptAssembler`. Add: read `.tool_filter` and `.max_tool_rounds` for tool filtering (currently done by a separate `classify_intent()` call that doesn't exist in this path — tool filtering is only in the edge path today).

**Edge path** (`core/agent/chat_loop.py`): Currently calls `classify_intent()` (System 1) and `build_context()` (System 3) separately. Replace with a single `IntentRouter.route()` call, then pass the result to:
- `context_manager.build_context(routing_decision=...)` — uses `.task_type`
- Tool filtering loop — uses `.tool_filter` and `.max_tool_rounds` (replaces `classify_intent()`)

Also fix: `detect_correction()` should force intent to "feedback" (last 2 turns), not "question" (full context). This affects `api/routers/chat.py` line ~2199.

**Files**: `core/agent/chat_loop.py`, `api/routers/chat.py`, `core/context/manager.py`, `core/context/intent_routing.py`

### Phase 4: Remove dead code

- Delete `classify_intent()` from `core/skills/intent_router.py`
- Delete `classify_task()` from `core/context/manager.py`
- Delete `IntentClassification` dataclass
- Remove `_LOCAL_TOOLS` import from `chat_loop.py` (moved into RoutingDecision)
- Update `pipeline_stages.py` `IntentFilterStage` to read from RoutingDecision

**Files**: `core/skills/intent_router.py`, `core/context/manager.py`, `core/agent/chat_loop.py`, `core/agent/pipeline_stages.py`

### Phase 5: Unified intent event

Replace the `stream_intent_classification` event with a single `routing_decision` event that captures all dimensions:

```python
event_logger.create_stream_event(
    event_type="routing_decision",
    content=json.dumps({
        "intent": decision.routing_result.intent,
        "confidence": decision.routing_result.confidence,
        "tier": decision.routing_result.tier,
        "tool_filter": decision.tool_filter.value,
        "max_tool_rounds": decision.max_tool_rounds,
        "task_type": decision.task_type.value,
        "topic_shift_score": decision.topic_shift_score,
        "threshold_used": decision.threshold_used,
        "query_hash": hashlib.sha256(query.encode()).hexdigest(),
    }),
)
```

---

## Pre-Requisite Fix: Fulltext Scoring in MemoryRetriever

The comment in `retriever.py` line 8-10 is wrong:

```
MO Fulltext Limitation: MATCH() AGAINST() can only be used in WHERE clause
for filtering, not in SELECT for arithmetic scoring.
```

MatrixOne **does** support `MATCH(...) AGAINST(...) AS score` in SELECT — the Python SDK's `FulltextSearchBuilder.with_score()` generates exactly this SQL:

```sql
SELECT *, MATCH(content) AGAINST('query' IN NATURAL_LANGUAGE) AS score
FROM mem_memories
WHERE MATCH(content) AGAINST('query' IN NATURAL_LANGUAGE)
ORDER BY score DESC
```

This returns a continuous BM25 relevance score, not a binary 0/1. The current retriever treats keyword as a binary signal (`kw_score = 1.0 if c.keyword_matched else 0.0`) — this should be replaced with the actual fulltext score from the DB.

**Action**: Phase 0 (before intent unification) — fix `_phase1` to SELECT the MATCH score and propagate it through `_Candidate.keyword_score: float` instead of `keyword_matched: bool`. This is independent of intent unification but improves retrieval quality.

---

## Open Design Questions (Resolved)

### Q1: feedback vs question — lost nuance

Both "feedback" and "question" currently map to `tool_filter=NONE, max_rounds=MAX`. But feedback sometimes needs special handling (e.g., lower history weight to avoid reinforcing the wrong answer).

**Resolution**: `ContextLoadingPlan` already differentiates them — feedback loads `history=2` (last 2 turns only), question loads full history. The tool_filter/max_rounds being the same is correct: feedback may need tools to re-execute. The nuance is preserved in the `plan`, not in tool filtering.

However, there's a subtlety: when the user says "不对" (wrong), the current `detect_correction()` forces intent to "question" with full context. This is too aggressive — it should force to "feedback" instead, which loads only the last 2 turns (the ones being corrected). Update the correction handler:

```python
# Before: force_intent="question" (loads everything)
# After:  force_intent="feedback" (loads last 2 turns — the correction target)
if detect_correction(query):
    force_intent = "feedback"
```

### Q2: topic_shift_score × task_type interaction

When topic_shift is high (>0.7), should task_type be forced to GENERAL?

**Resolution**: No. High topic shift means the user changed subjects, but the new subject still has a task type. "We were debugging X, now review this PR" has high topic_shift but task_type should be CODE_REVIEW, not GENERAL.

What topic_shift *should* do is reduce history weight (already implemented in `RelevanceScorer.apply_topic_shift_adjustment()`). The interaction rule:

| topic_shift | task_type | Effect |
|-------------|-----------|--------|
| low (<0.3) | any | Normal budget ratios for task_type |
| medium (0.3-0.7) | any | Reduce history weight by shift_score × 0.5 |
| high (>0.7) | any | Reduce history weight by shift_score × 0.5, but keep task_type as classified |

This is already the behavior. The design doc should document it explicitly rather than leave it ambiguous.

### Q3: Tier 0 keyword bloat

Merging three keyword sets into one `Tier0Engine.classify()` risks a maintenance nightmare as patterns grow.

**Resolution**: Keep keyword sets as separate, named registries. Tier 0 runs them in sequence with early-exit:

```python
class KeywordRegistry:
    """Single-dimension keyword matcher. Supports hot-reload from YAML config."""

    def __init__(self, name: str, keywords: dict[str, list[str]]):
        self.name = name
        self._patterns = {k: [_compile(w) for w in words] for k, words in keywords.items()}

    @classmethod
    def from_yaml(cls, name: str, path: str) -> "KeywordRegistry":
        """Load from config/intent_keywords.yaml — enables hot-reload without code change."""
        import yaml
        with open(path) as f:
            data = yaml.safe_load(f)
        return cls(name, data[name])

    def match(self, query: str, history_len: int = 0) -> RegistryResult: ...


class Tier0Engine:
    # Each registry is a separate concern, independently testable.
    # Default: hardcoded for zero-config startup.
    # Production: load from config/intent_keywords.yaml for hot-reload.
    _registries: list[tuple[str, KeywordRegistry]] = [
        ("tool_filter", _TOOL_FILTER_KEYWORDS),   # EXTERNAL_FETCH / CONVERSATIONAL
        ("intent", _INTENT_KEYWORDS),              # preference / command / feedback / question
        ("task_type", _TASK_TYPE_KEYWORDS),         # CODE_REVIEW / DEBUGGING / PLANNING
    ]

    def classify(self, query: str, history_len: int = 0) -> Tier0Result:
        results = {}
        for name, registry in self._registries:
            results[name] = registry.match(query, history_len)
        return Tier0Result(**results)
```

Each `KeywordRegistry` is a standalone class with its own keyword sets, tests, and match logic. Adding keywords to one registry doesn't touch the others. The engine just orchestrates. Registries default to hardcoded keywords (zero-config) but support `from_yaml()` for config-driven hot-reload — consistent with the project's config-driven style.

### Q4: Conflict resolution — "review code" + "search online"

If a query matches both EXTERNAL_FETCH ("search online") and CODE_REVIEW ("review"), which wins?

**Resolution**: They're on different dimensions and don't conflict:
- `tool_filter=LOCAL_BLOCKED` (from "search online") — controls *which tools* are available
- `task_type=CODE_REVIEW` (from "review code") — controls *token budget ratios*

Both can be true simultaneously: "search online for code review best practices" should block local tools AND use code_review budget ratios.

The only real conflict is within the same dimension. Priority rules:

**tool_filter conflicts** (same dimension):
1. CONVERSATIONAL > EXTERNAL_FETCH > NONE (most restrictive wins)
2. But: if code-context keywords are present, suppress EXTERNAL_FETCH → NONE (existing behavior)

**intent conflicts** (same dimension):
1. Higher confidence wins
2. Tie: prefer the intent with more matched keywords
3. Still tied: fall through to Tier 1

**task_type conflicts** (same dimension):
1. Higher keyword match score wins
2. Tie: GENERAL (safe default)

---

## What NOT to Do

- **Don't create a fourth `Intent` dataclass** — we already have three. The answer is fewer abstractions, not more.
- **Don't merge the taxonomies** — "preference/command/feedback/question" and "CODE_REVIEW/DEBUGGING/PLANNING/GENERAL" are orthogonal dimensions. A user can give "feedback" about a "debugging" task. Keep them as separate fields on one object.
- **Don't move Tier 1 LLM classification into chat_loop** — it belongs in the routing layer. chat_loop should receive a decision, not make one.

---

## Migration Safety

Each phase is independently deployable and backward-compatible:

- Phase 1: New fields have defaults → zero behavior change
- Phase 2: Tier 0 produces richer output → downstream ignores new fields until Phase 3
- Phase 3: Callers read new fields → old code paths become dead but still work
- Phase 4: Dead code removal → pure cleanup
- Phase 5: Event schema change → old events still readable (superset of old fields)

Tests at each phase: run full suite (`make dev-test-keep`), verify no regressions.

---

## Testing Strategy

### Unit tests (target: >95% coverage on new code)

Each `KeywordRegistry` gets its own test class:
- `test_tool_filter_registry.py` — EXTERNAL_FETCH, CONVERSATIONAL, code-context suppression
- `test_intent_registry.py` — preference, command, feedback, question
- `test_task_type_registry.py` — CODE_REVIEW, DEBUGGING, PLANNING, GENERAL fallback

`Tier0Engine` combination tests covering Q4 conflict scenarios:
- Cross-dimension co-occurrence: "search online for code review" → tool_filter=LOCAL_BLOCKED + task_type=CODE_REVIEW
- Same-dimension conflict: "hello, run this command" → CONVERSATIONAL > NONE (most restrictive wins)
- Code-context suppression: "search online for this function" → EXTERNAL_FETCH suppressed → NONE

### Regression test queries (10 representative cases)

| # | Query | Expected intent | Expected tool_filter | Expected task_type |
|---|-------|----------------|---------------------|-------------------|
| 1 | "hello" | *(conversational)* | ALL_BLOCKED | GENERAL |
| 2 | "run pytest" | command | NONE | GENERAL |
| 3 | "不对，应该用 LEFT JOIN" | feedback | NONE | DEBUGGING |
| 4 | "review this PR" | question | NONE | CODE_REVIEW |
| 5 | "search online for best practices" | question | LOCAL_BLOCKED | GENERAL |
| 6 | "debug this traceback" | question | NONE | DEBUGGING |
| 7 | "记住我喜欢用 vim" | preference | ALL_BLOCKED | GENERAL |
| 8 | "plan the migration roadmap" | question | NONE | PLANNING |
| 9 | "search online for code review tools" | question | LOCAL_BLOCKED | CODE_REVIEW |
| 10 | "wrong, actually use async" | feedback | NONE | GENERAL |

These are codified as parametrized pytest cases, run in both unit (mocked) and integration (real DB) suites.

### Phase 0 specific tests

- Verify `_phase1` returns continuous BM25 score (not binary 0/1)
- Verify score ordering: exact match > partial match > no match
- Regression: existing retrieval integration tests must not change ranking for queries where keyword was already binary-matched

---

## Performance Estimate

| Metric | Before (3 passes) | After (1 pass) | Improvement |
|--------|-------------------|----------------|-------------|
| Keyword matching passes | 3 | 1 | -67% |
| Regex compilations at startup | 3 separate caches | 1 unified cache | -67% memory |
| Per-query classification time | ~0.8ms (3 × ~0.25ms) | ~0.3ms | ~60% faster |
| Total routing latency | Unchanged (dominated by Tier 1 LLM ~180ms when triggered) | Unchanged | — |

The classification time improvement is marginal in absolute terms (<1ms) since Tier 0 is already sub-millisecond. The real value is architectural: single source of truth eliminates inconsistency bugs and reduces the surface area for intent-related regressions.
