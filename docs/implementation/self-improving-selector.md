# Self-Improving Skill Selector — Implementation

> **Last Updated**: 2026-02-28
> **Design**: [Skills and Tools §3](../design/skills-and-tools.md#3-skill-selection-pipeline)
> **Usage Guide**: [Multi-Dimensional Learning Guide](../guides/multi-dimensional-learning-guide.md)

Implementation details for the self-improving skill selection pipeline.

## Module Map

```
core/skills/pipeline.py                  ← Unified entry point (SkillPipeline); audit logic inlined here
core/skills/modern_selector.py           ← Semantic retrieval + LLM ranking (internal)
core/skills/self_improving_selector.py   ← Learning engine (internal)
core/skills/selector.py                  ← SkillMetadata + rule-based retrieval (internal)
core/skills/skill_index.py               ← Cosine similarity embedding index
core/skills/learning_signals.py          ← SignalType, SignalWeights
core/skills/learning_config.py           ← Per-signal decay/threshold config
core/skills/learning_similarity.py       ← Query pattern similarity
core/skills/procedural_memory.py         ← Type bridge: learnings → Memory objects
core/evaluation/regression_gate.py       ← Gate validation
```

> `AuditableSkillSelector` no longer exists — audit event creation is inlined in `pipeline.py`.

## Semantic Retrieval

`ModernSkillSelector` uses an in-memory cosine similarity index (`core/skills/skill_index.py`) to match user queries to skill descriptions:

1. Embed all skill descriptions at startup (via `EmbeddingService`)
2. Embed user query at request time
3. Rank by cosine similarity, apply token budget for progressive disclosure

Falls back to keyword matching when no embedding function is available.

### Framework Field Stripping

`SkillInput` defines framework-injected fields (`user_id`, `session_id`, `repo_id`) that are auto-injected by the executor. These are stripped from the LLM-visible tool schema via `_FRAMEWORK_FIELDS` ClassVar so the LLM never sees or fills them.

## Learning Cycle

> For signal types, multi-factor scoring formula, and safety mechanisms, see [Skills and Tools §3](../design/skills-and-tools.md#3-skill-selection-pipeline).

```
Signals accumulate in skill_learning_signals table
    → mo-agent skill learn --days 7
    → Analyze: group signals by query pattern
    → Propose: correction rules (wrong_skill → correct_skill)
    → Validate: RegressionGate replays golden sessions
    → Deploy: store in skill_selection_learnings, apply at selection time
```

Corrections are applied as score adjustments during selection — boosting correct skills and penalizing wrong ones for matching query patterns.

## Procedural Memory Bridge

`core/skills/procedural_memory.py` provides a type-layer adapter that converts `skill_selection_learnings` rows into `Memory` domain objects. This enables the Skill Selector to use memory-system APIs (governance, confidence decay, trust tiers) without duplicating data.

**Design boundary**: skill selection learnings are Skill Selector internal correction rules, NOT general-purpose procedural memory. The bridge is consumed only during skill selection — it is NOT injected into `MemoryRetriever`.

## API

| Endpoint | Purpose |
|----------|---------|
| `POST /api/v1/learning/trigger` | Trigger learning cycle |
| `GET /api/v1/learning/signals` | List recorded signals |
| `GET /api/v1/learning/stats` | Learning statistics |
| `POST /api/v1/learning/feedback` | Submit feedback signal |
| `GET /api/v1/learning/health` | Learning system health |

## Database Tables

- `skill_selection_events` — every selection decision with query, selected skills, method
- `skill_selection_learnings` — learned correction rules with confidence and evidence count
- `skill_learning_signals` — raw feedback signals per execution
- `gate_results` — regression gate verdicts for learning changes

## Usage

```python
from core.skills.pipeline import SkillPipeline

pipeline = SkillPipeline(db=db, llm_client=llm_client)

# Select skills
result = pipeline.get_tools_schema(query="Create a PR", session_id=session_id)

# Record feedback after execution
pipeline.record_feedback(result.event_id, SignalType.EXECUTION_TIME, {"ms": 150})
```

```bash
# CLI: trigger learning
mo-agent skill learn --days 7

# API: submit feedback
POST /api/v1/learning/feedback
{"event_id": "evt_123", "feedback_type": "wrong_skill", "correct_skills": ["github_create_pr"]}
```
