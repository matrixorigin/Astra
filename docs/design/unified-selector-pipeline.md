# Unified Selector Pipeline

> **Status**: Approved — replaces 5 selector classes with 1 pipeline  
> **Parent**: [Skills and Tools](skills-and-tools.md) §1  
> **Implements**: [Agents and Orchestration](agents-and-orchestration.md) §1 (ChatLoop skill selection)

---

## 1. Problem

Five selector classes exist with overlapping responsibilities:

| Class | Location | What it does |
|-------|----------|-------------|
| `SkillSelector` | `core/skills/selector.py` | Keyword matching, dependency resolution |
| `ModernSkillSelector` | `core/skills/modern_selector.py` | LLM function calling, tools schema |
| `AuditableSkillSelector` | `core/skills/auditable_selector.py` | Wraps Modern + audit events |
| `SelfImprovingSelector` | `core/skills/self_improving_selector.py` | Learning from failure signals |
| `AgentSkillSelector` | `core/agent/selector.py` | Wrapper that tries to integrate all above |

**The actual bug**: ChatLoop calls `selector.get_tools_schema()` which delegates to `ModernSkillSelector.get_tools_schema()` — bypassing audit, learning, and validation entirely. The audit and learning code exists but is never invoked during normal conversation flow.

## 2. Solution

Replace 5 classes with a single `SkillPipeline` that ChatLoop calls for every selection. The pipeline has 3 stages that always run (audit/learning can be disabled via config, not by using a different class).

```
┌─────────────────────────────────────────────────────────┐
│                     SkillPipeline                        │
│                                                          │
│  ┌──────────────────────────────────────────────────┐   │
│  │ Stage 1: RETRIEVE + RANK                         │   │
│  │  Rule-based retrieval → LLM function calling     │   │
│  │  → Apply learned corrections                     │   │
│  │  Output: tools_schema + candidate metadata       │   │
│  └──────────────────────────────────────────────────┘   │
│                         │                                │
│  ┌──────────────────────▼───────────────────────────┐   │
│  │ Stage 2: AUDIT                                   │   │
│  │  Snapshot context → Record selection event        │   │
│  │  Output: event_id (for feedback linkage)          │   │
│  └──────────────────────────────────────────────────┘   │
│                         │                                │
│  ┌──────────────────────▼───────────────────────────┐   │
│  │ Stage 3: FEEDBACK (post-execution, async)        │   │
│  │  Collect signals → Batch write                    │   │
│  │  Learning cycle runs periodically, not inline     │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

### Key design decisions

1. **`get_tools_schema()` remains the primary interface** — ChatLoop needs tools schema for LLM function calling. The pipeline enriches this call with audit + learning, not replaces it.

2. **Learning is separate from selection** — `SkillPipeline.get_tools_schema()` applies learned corrections synchronously. Learning cycle (`learn()`) runs asynchronously via scheduler or API call. They share state through the DB, not through method calls on the same object.

3. **Feedback is batched** — `record_feedback()` writes to an in-memory buffer, flushed periodically. No synchronous DB write per tool execution.

4. **No internal implementation leaks** — Callers never see `ModernSkillSelector` or `SelfImprovingSelector`. The pipeline is the only public interface.

## 3. Interface

```python
class SkillPipeline:
    """Unified skill selection: retrieve → audit → feedback."""

    def __init__(
        self,
        db: Session,
        llm_client: LLMClient,
        *,
        audit: bool = True,
        learning: bool = True,
    ): ...

    def get_tools_schema(
        self,
        query: str,
        session_id: str,
        *,
        max_candidates: int = 5,
    ) -> ToolsResult:
        """Select skills and return tools schema for LLM.

        Stage 1: Retrieve candidates (rule-based + LLM ranking)
                 Apply learned corrections
        Stage 2: Record audit event with context snapshot

        Returns ToolsResult with schema + event_id.
        """

    def record_feedback(
        self,
        event_id: str,
        signal: SignalType,
        data: dict,
    ) -> None:
        """Buffer a feedback signal (async flush)."""

    def learn(self, *, days: int = 7) -> LearningResult:
        """Run learning cycle. Called by scheduler, not by ChatLoop."""

    def stats(self) -> dict: ...


@dataclass
class ToolsResult:
    """Result of skill selection."""
    tools: list[dict]       # OpenAI tools schema, ready for LLM
    event_id: str | None    # Audit event ID (None if audit disabled)
    candidates: int         # Number of candidates considered
```

## 4. ChatLoop Integration

### Before (current)

```python
# core/agent/chat_loop.py line ~155
tools_schema = self.selector.get_tools_schema(
    query=user_input, max_candidates=max_candidates
)
# No audit. No learning. No feedback.
```

### After

```python
result = self.pipeline.get_tools_schema(
    query=user_input,
    session_id=session_id,
    max_candidates=max_candidates,
)
tools_schema = result.tools

# After each tool execution:
self.pipeline.record_feedback(
    result.event_id, SignalType.EXECUTION_TIME, {"ms": elapsed}
)
```

The change to ChatLoop is minimal — replace `self.selector` with `self.pipeline`, use `result.tools` instead of the raw return, add one `record_feedback()` call after execution.

## 5. Migration

### Phase 1: Create SkillPipeline (new file)

Create `core/skills/pipeline.py`:
- Compose `SkillSelector` (retrieval), `ModernSkillSelector` (LLM ranking), `SelfImprovingSelector` (corrections) internally
- Expose only `get_tools_schema()`, `record_feedback()`, `learn()`, `stats()`
- Audit event creation inlined (no separate AuditableSkillSelector)

### Phase 2: Wire into ChatLoop

- `ChatLoop.__init__` accepts `pipeline: SkillPipeline` instead of `selector`
- `cli/mo_agent.py` creates `SkillPipeline` instead of `AgentSkillSelector`
- `api/routers/streaming.py` same change
- `api/routers/learning.py` calls `pipeline.learn()` and `pipeline.stats()`

### Phase 3: Delete old code

Remove after all tests pass:
- `core/skills/selector.py` → retrieval logic moved into pipeline
- `core/skills/auditable_selector.py` → audit logic moved into pipeline
- `core/agent/selector.py` → replaced by pipeline

Keep (used internally by pipeline):
- `core/skills/modern_selector.py` — LLM function calling engine
- `core/skills/self_improving_selector.py` — learning engine
- `core/skills/learning_signals.py` — signal types
- `core/skills/regression_gate.py` — gate validation

## 6. What This Does NOT Change

- **Skill registration** — `core/skills/registry.py` unchanged
- **Skill execution** — `core/agent/executor.py` unchanged
- **Tool mocking** — `core/skills/mocking.py` unchanged
- **Built-in skills** — `core/skills/builtin.py`, `extended.py` unchanged
- **Delegation** — `core/skills/delegation.py` unchanged

The pipeline only unifies the *selection* path. Everything downstream of "which skill to call" stays the same.
