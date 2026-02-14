# Evaluation and Evolution

> **Status**: Core Design — single source of truth for quality measurement, testing, and self-improvement  
> **Last Updated**: 2026-02-14

---

## The Industry Direction

The field is converging on a key insight: **agent evaluation is fundamentally different from traditional software testing.** Outputs are stochastic, multi-step workflows create cascading errors, and quality is subjective. The answer is evaluation-driven iteration: Define → Test → Diagnose → Fix, as a repeatable engineering loop.

mo-agent-engine's unique advantage: we have the **data infrastructure** (time travel, snapshots, causal chains) to make this loop automated and auditable.

---

## 1. Quality Measurement

### Multi-Dimensional Scoring

A single "quality score" is insufficient. Agent quality has multiple dimensions:

```
quality_assessment = {
  task_completion:  Did the agent accomplish what was asked?     (0-5)
  factual_accuracy: Are the claims correct?                      (0-5)
  helpfulness:      Was the response useful to the user?         (0-5)
  safety:           Did the agent follow guardrails?             (0-5)
  efficiency:       Was the token/cost usage reasonable?          (0-5)
  
  overall_score:    weighted_average(above)                      (0-5)
}
```

### Scoring Sources

| Source | When | How |
|--------|------|-----|
| **User feedback** | After response delivery | Thumbs up/down, 1-5 rating, text feedback |
| **Auto-metric** | Immediately after chain completes | Rule-based: task completion check, format validation |
| **LLM-as-judge** | Async, batch | Small model evaluates response quality against criteria |
| **Human label** | Periodic review | Expert annotation for training data quality |

### Early Auto-Metrics

Don't wait for user feedback. Implement lightweight auto-metrics from day one:

```python
auto_metrics = {
  "task_completed": did_agent_use_expected_skill_type(chain),
  "format_valid": does_response_match_expected_format(chain),
  "no_hallucination": hallucination_firewall_passed(chain),
  "within_budget": actual_cost <= estimated_cost * 1.2,
  "reasonable_length": 50 < response_tokens < 2000,
}
```

These populate `quality_score` and `training_eligible` without human intervention.

---

## 2. Evaluation-Driven Iteration

### The Loop

```
DEFINE: What does "good" look like for this agent/skill/prompt?
  → Golden sessions, quality criteria, acceptance thresholds

TEST: Run the agent against evaluation suite
  → Replay golden sessions, compute metrics, compare to baseline

DIAGNOSE: Where did it fail and why?
  → Context snapshot analysis, causal chain inspection, memory audit

FIX: Adjust the input that caused the failure
  → Prompt revision, skill update, memory correction, context budget tuning

VALIDATE: Prove the fix works without breaking other things
  → Regression gate in sandbox

DEPLOY: Ship with confidence
  → Gate passes → activate change
```

### Golden Session Selection

```sql
-- Automatically curate golden sessions
SELECT session_id, AVG(quality_score) as avg_score, COUNT(*) as event_count
FROM conversation_events
WHERE quality_score >= 4.0
  AND training_eligible = TRUE
  AND created_at > NOW() - INTERVAL 30 DAY
GROUP BY session_id
HAVING event_count >= 3  -- Multi-turn conversations
ORDER BY avg_score DESC
LIMIT 50
```

Golden sessions are diverse (different task types, skill usage patterns) and high-quality. They form the regression test suite.

### Agent Evaluation ≠ Single-Turn Evaluation

Following Braintrust's insight: agent evaluation must account for multi-step workflows where errors cascade. Evaluate at three levels:

```
Step-level:  Did each individual tool call succeed?
Chain-level: Did the full causal chain produce the right outcome?
Session-level: Did the overall conversation satisfy the user?
```

A step can succeed while the chain fails (wrong skill selected). A chain can succeed while the session fails (right answer to wrong question).

---

## 3. Replay Gating

### Automated Quality Gate

Every change (prompt, skill, config, model) must pass through replay gating before production:

```
Change detected (skill_version_changed | prompt_template_changed | ...)
  │
  ▼
Create snapshot of current production state
  │
  ▼
Load golden sessions (top 50 by quality_score)
  │
  ▼
Create sandbox from snapshot
  │
  ▼
Apply change to sandbox (new prompt/skill/config)
  │
  ▼
Replay golden sessions in sandbox
  (with tool mocking — no real API calls)
  │
  ▼
Compute metrics:
  - Error rate (must be < 5%)
  - Score delta on high-score sessions (must not regress)
  - Score delta on low-score sessions (should improve)
  - Latency delta
  - Token efficiency
  │
  ▼
Pass/Fail decision
  │
  ├── Pass → Activate change in production
  └── Fail → Reject change, notify developer with failure details
  │
  ▼
Record gate_result with full lineage
  │
  ▼
Cleanup sandbox
```

### Gate Results with Lineage

```sql
INSERT INTO gate_results (
  gate_id, change_type, change_id,
  snapshot_used, sessions_tested,
  error_rate, passed, metrics
) VALUES (
  'gate_01...', 'prompt_change', 'code_review@v3',
  'snapshot_20260214_1400', 50,
  0.02, TRUE,
  '{"score_delta": +0.15, "latency_delta_ms": -200, "token_delta": -500}'
);
```

Every gate result is auditable: which snapshot, which sessions, what metrics, pass/fail.

---

## 4. Prompt Evolution

### The Problem

Prompt engineering is trial-and-error with no scientific method. Teams change prompts and deploy. Did it break existing cases? No one knows until users complain.

### The Solution: Branch-Based Experimentation

```
1. Identify low-scoring causal chains (quality_score < 3.0)
2. Analyze failure patterns (wrong skill? insufficient context? bad instructions?)
3. Generate candidate prompt revision
   - Manual: developer writes new version
   - Semi-auto: LLM proposes revision based on failure analysis
   - Auto: DSPy/TextGrad optimization on historical chains
4. Create sandbox branch
5. Replay low-score AND high-score chains with candidate
6. Compute quality delta
7. If improvement > threshold AND no regression: propose merge
8. Human review → approve → activate new version
```

### Prompt as Versioned Data

```sql
-- Every prompt change is versioned
INSERT INTO prompt_templates (template_id, version, content, effective_at, is_active)
VALUES ('code_review', 'v3', '...new prompt...', NOW(), FALSE);
-- is_active = FALSE until gate passes

-- Events reference the exact version used
UPDATE conversation_events SET prompt_template_id = 'code_review@v3'
WHERE ...;
-- Historical events still reference v2 — never changed
```

---

## 5. Self-Improving Agents (Meta-Learning)

### The Closed Loop

The platform has multiple self-improvement mechanisms that form a unified meta-learning loop:

```
OBSERVE: quality_score < threshold on a causal chain
    ↓
DIAGNOSE: Which input was the bottleneck?
    - Wrong prompt version? → Prompt evolution pipeline
    - Missing skill? → Skill gap detection
    - Insufficient context? → Context budget tuning
    - Stale knowledge? → Knowledge regression detection
    - Wrong skill selected? → SelfImprovingSelector
    ↓
PROPOSE: Generate candidate adjustment
    ↓
VALIDATE: Replay failing chain in sandbox with the candidate
    ↓
DEPLOY: If improvement > threshold, propose change
    - Config changes: auto-deploy
    - Prompt changes: human-approved
    - Skill changes: regression gate required
    ↓
RECORD: Store the learning signal for future pattern matching
```

### Already Implemented

- **SelfImprovingSelector**: Learns from historical skill selection failures using time-travel replay
- **SkillSelectionRegressionGate**: Validates selector changes before deployment
- **AuditableSkillSelector**: Records every selection decision with full context

### The Generalization

Meta-learning generalizes `SelfImprovingSelector` to ALL versioned inputs:

| Input | Current | Meta-Learning |
|-------|---------|---------------|
| Skill selection | SelfImprovingSelector | ✅ Already learning |
| Prompt | Manual iteration | Auto-propose from failure patterns |
| Context budget | Fixed allocation | Task-aware dynamic allocation |
| Model routing | Static rules | Cost-quality optimization from historical data |
| Memory retrieval | Fixed weights | Relevance weight tuning from feedback |

### Confidence Calibration

```
Pre-delivery confidence_score vs post-delivery quality_score:

If confidence consistently > quality: system is overconfident → lower weights
If confidence consistently < quality: system is underconfident → raise weights
If calibration error > threshold: flag for uncertainty model tuning

This measures how well the system knows what it doesn't know.
```

---

## 6. Training Data Pipeline

### From Production to Fine-Tuning

```
Production events
  → Quality filtering (score >= 4.0, training_eligible = TRUE)
  → SFT pair extraction (user_query → llm_response from causal chains)
  → Dataset snapshot (named, versioned, reproducible)
  → Contamination check (train/test overlap detection)
  → Export (JSONL/Parquet)
  → Fine-tuning pipeline
  → New model version
  → Regression gate
  → Production
```

### Online Fine-Tuning Trigger

When `training_eligible` events accumulate beyond a threshold per user/tenant:

```
1. Extract user-specific training data
2. Run LoRA fine-tuning (or similar)
3. Validate in sandbox (replay sample chains with new adapter)
4. If improvement confirmed: enable for that user/tenant
5. Monitor quality_score for regression
```

This enables "agent evolves with user interaction" while keeping risk bounded by sandbox validation.

---

## 7. CI/CD Integration

### Replay Gate in CI

```yaml
# .github/workflows/agent-quality.yml
on:
  pull_request:
    paths:
      - 'prompts/**'
      - 'skills/**'
      - 'config/**'

jobs:
  replay-gate:
    runs-on: ubuntu-latest
    steps:
      - name: Run replay gate
        run: mo-agent replay-gate run --sessions 50 --threshold 0.95
      
      - name: Comment results on PR
        if: always()
        run: mo-agent replay-gate report --format github-comment
```

Every prompt/skill change gets automated quality validation before merge. PR comment shows metrics. Merge blocked if gate fails.

---

## References

- [Braintrust: AI Agent Evaluation Framework](https://www.braintrust.dev/articles/ai-agent-evaluation-framework)
- [PromptLayer: Practical Guide to AI Agents Evaluation](https://blog.promptlayer.com/ai-agents-evaluations/)
- [Maxim: AI Observability Production-Ready Guide](https://www.getmaxim.ai/articles/ai-observability-and-monitoring-a-production-ready-guide-for-reliable-ai-agents/)
- [Arxiv: Evaluation-Driven Iteration for LLM Applications](https://arxiv.org/html/2601.22025v1)
- [AI Agents 2026: Practical Architecture](https://www.andriifurmanets.com/blogs/ai-agents-2026-practical-architecture-tools-memory-evals-guardrails)

Content was rephrased for compliance with licensing restrictions.
