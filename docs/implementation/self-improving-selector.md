# Self-Improving Skill Selector

> **Last Updated**: 2026-02-21

How the skill selection pipeline learns from feedback to improve over time.

## Architecture

```
User query
    → SkillPipeline (unified entry point)
        → ModernSkillSelector: semantic retrieval (cosine similarity index)
        → SelfImprovingSelector: apply learned corrections
        → AuditableSkillSelector: record selection decision
    → Tools schema returned to LLM
    → After execution: feedback signals recorded
    → Periodic: learning cycle analyzes signals → proposes corrections → gate validates
```

**Module**: `core/skills/pipeline.py` → `core/skills/modern_selector.py` → `core/skills/self_improving_selector.py`

## Skill Selection

### Semantic Retrieval

`ModernSkillSelector` uses an in-memory cosine similarity index (`core/skills/skill_index.py`) to match user queries to skill descriptions:

1. Embed all skill descriptions at startup (via `EmbeddingService`)
2. Embed user query at request time
3. Rank by cosine similarity, apply token budget for progressive disclosure:
   - Tier 1: skill names + descriptions (always included)
   - Tier 2: full parameter schemas (budget-gated)

Falls back to keyword matching when no embedding function is available.

### Framework Field Stripping

`SkillInput` defines framework-injected fields (`user_id`, `session_id`, `repo_id`) that are auto-injected by the executor. These are stripped from the LLM-visible tool schema via `_FRAMEWORK_FIELDS` ClassVar so the LLM never sees or fills them.

## Learning Signals

Four signal types drive learning:

| Signal | Trigger | Example |
|--------|---------|---------|
| `WRONG_SKILL` | User corrects skill choice | "I wanted create_pr, not list_prs" |
| `SLOW_EXECUTION` | Execution > 5000ms | Skill took too long |
| `HIGH_COST` | Cost > $0.10 | Expensive LLM calls in skill |
| `LOW_SATISFACTION` | User rating < 3 | Poor result quality |

## Learning Cycle

```
Signals accumulate in skill_learning_signals table
    → mo-agent skill learn --days 7
    → Analyze: group signals by query pattern
    → Propose: correction rules (wrong_skill → correct_skill)
    → Validate: RegressionGate replays golden sessions
    → Deploy: store in selector_learnings, apply at selection time
```

Corrections are applied as score adjustments during selection — boosting correct skills and penalizing wrong ones for matching query patterns.

## Multi-Factor Scoring

Selection score combines multiple dimensions with configurable weights:

```
score = accuracy_weight  × accuracy_score
      + speed_weight     × speed_score
      + cost_weight      × cost_score
      + satisfaction_weight × satisfaction_score
```

Weights and decay rates are configurable per signal type.

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
- `selector_learnings` — learned correction rules with confidence and evidence count
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
