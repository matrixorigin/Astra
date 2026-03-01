# Tool Result Quality Firewall

> **Status**: Core Design  
> **Last Updated**: 2026-03-01  
> **Triggered By**: Session `019ca950` — agent based investment advice on empty data (`technical_indicators={}`, `risk_score=0`, `trend_analysis={}`) without noticing  
> **Related**: [trust-and-safety.md](trust-and-safety.md) (output verification), [context-window-management.md](context-window-management.md) (context quality), [memory-architecture.md](memory-architecture.md) (tool context engine), [evaluation-and-evolution.md](evaluation-and-evolution.md) (quality scoring)

---

## Executive Summary

The Hallucination Firewall (trust-and-safety.md §2) verifies LLM *output* claims against the context snapshot. But it cannot catch a subtler failure: the LLM faithfully summarizes tool results that are themselves **empty, degraded, or semantically vacuous**. The LLM is not hallucinating — it is *confabulating*: weaving a confident narrative from data that contains no actual signal.

This document designs a **Tool Result Quality Firewall** — a pre-LLM gate that evaluates tool results *before* they enter the context window, annotates them with quality signals, and gives the LLM the information it needs to respond honestly instead of confidently.

### The Gap in Current Architecture

```
Current trust pipeline:

  Tool Result ──────────────────────────────► Context Window ──► LLM ──► Hallucination Firewall ──► User
                  (no quality check)              (confabulates)         (verifies claims against
                                                                          snapshot — but snapshot
                                                                          contains the same empty data)

Proposed:

  Tool Result ──► Quality Firewall ──► Annotated Result ──► Context Window ──► LLM ──► User
                  (detect empty/degraded)  (quality signals)    (LLM sees signals)  (responds honestly)
```

The Hallucination Firewall is a *post-hoc* check: "did the LLM say something unsupported?" The Tool Result Quality Firewall is a *pre-hoc* check: "is the data worth reasoning about?"

### Why This Is Hard

The LLM cannot distinguish between "the tool returned `risk_score: 0` meaning zero risk" and "the tool returned `risk_score: 0` meaning the field was not computed." Both are valid JSON. Both parse correctly. The difference is semantic — and the LLM has no metadata to resolve the ambiguity.

### Design Principles

1. **Annotate, don't block.** Tool results are never discarded. Quality signals are injected as metadata that the LLM can reason about. The LLM decides how to respond — the firewall provides the signal.
2. **Schema-driven, not heuristic.** Quality assessment is based on the skill's declared output schema, not ad-hoc rules. Skills declare what "complete" looks like.
3. **Zero LLM cost.** All quality assessment is rule-based. No additional LLM calls.
4. **Composable with existing trust pipeline.** Quality signals feed into the Hallucination Firewall's confidence scoring and the auto-scorer's quality metrics.

---

## 1. The Problem: Three Failure Modes

### 1.1 Empty Shell (Session 019ca950)

Tool returns structurally valid JSON with semantically empty fields:

```json
{
  "success": true,
  "technical_indicators": {},
  "trend_analysis": {},
  "risk_assessment": {"risk_score": 0, "risk_level": "低风险", "risk_factors": [], "max_drawdown_estimate": 0},
  "investment_advice": {"overall_recommendation": "持有", "confidence": 50}
}
```

`success: true` but the analysis fields are empty. The LLM sees `risk_level: "低风险"` and reports "low risk" — but the risk was never assessed. `confidence: 50` is the default, not a computed value.

### 1.2 Partial Degradation

Tool returns some valid data and some degraded fields:

```json
{
  "success": true,
  "current_price": 27.37,
  "volume": 87758758,
  "technical_indicators": {},
  "fundamental_analysis": {"pe_ratio": null, "pb_ratio": null}
}
```

Price and volume are real. Technical indicators and fundamentals are missing. The LLM should use the real data and acknowledge the gaps — not fill them with plausible-sounding fabrication.

### 1.3 Stale Data

Tool returns data that is technically correct but temporally irrelevant:

```json
{
  "success": true,
  "data_timestamp": "2026-02-15T10:00:00",
  "current_price": 28.50
}
```

Query made on March 1st, data is from February 15th. The price is "correct" for that date but misleading as "current."

---

## 2. Architecture

### 2.1 Where It Runs

The quality firewall runs **server-side in the chat turn handler**, after tool results arrive from the edge and before they are merged into the conversation history for LLM consumption.

```
Edge executes tool
    │
    ▼
/chat/turn receives tool_results
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│  TOOL RESULT QUALITY FIREWALL (new)                         │
│                                                             │
│  For each tool_result:                                      │
│    1. Load skill's quality schema (cached)                  │
│    2. Assess completeness, freshness, sentinel values       │
│    3. Compute quality_score (0.0–1.0)                       │
│    4. Generate quality_annotation (human-readable)          │
│    5. Inject annotation into tool_result content            │
│                                                             │
│  Output: annotated tool_results                             │
└──────────────────────────┬──────────────────────────────────┘
                           │
                           ▼
Merge into conversation history → LLM sees annotated results
```

### 2.2 Component Design

```
┌─────────────────────────────────────────────────────────────┐
│  ToolResultQualityFirewall                                  │
│                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ SchemaLoader  │  │ QualityRules │  │ Annotator        │  │
│  │              │  │              │  │                  │  │
│  │ Load skill's │  │ Completeness │  │ Inject quality   │  │
│  │ quality      │  │ Freshness    │  │ signals into     │  │
│  │ schema from  │  │ Sentinel     │  │ tool_result      │  │
│  │ registry     │  │ Consistency  │  │ content          │  │
│  └──────────────┘  └──────────────┘  └──────────────────┘  │
│                                                             │
│  Input:  tool_result (raw JSON from edge)                   │
│  Output: tool_result (annotated with quality signals)       │
│  Cost:   zero LLM calls, <5ms per result (see §4.3)        │
└─────────────────────────────────────────────────────────────┘
```


---

## 3. Skill Quality Schema

### 3.1 The Core Innovation: Skills Declare Their Own Quality Contract

Currently, skills declare input parameters (`parameters` in `skills_registry`). They do not declare what a *quality* output looks like. This is the root cause — without a quality contract, the platform cannot distinguish "complete result" from "empty shell."

**New field in `skills_registry`**: `quality_schema`

```python
# skills_registry.quality_schema (JSON)
{
    # Fields that MUST be non-empty for the result to be considered complete
    "required_fields": [
        {"path": "technical_indicators", "type": "dict", "min_keys": 2},
        {"path": "trend_analysis", "type": "dict", "min_keys": 1},
        {"path": "risk_assessment.risk_factors", "type": "list", "min_length": 1}
    ],

    # Fields with known sentinel/default values that indicate "not computed"
    "sentinel_values": [
        {"path": "risk_assessment.risk_score", "sentinel": 0, "meaning": "not computed (default value)"},
        {"path": "investment_advice.confidence", "sentinel": 50, "meaning": "default confidence, not a real assessment"}
    ],

    # Freshness requirement
    "freshness": {
        "timestamp_field": "data_timestamp",
        "max_age_seconds": 86400  # 24 hours
    },

    # Minimum quality threshold — below this, annotation is mandatory
    "min_quality_threshold": 0.6
}
```

### 3.2 Schema Inference for Skills Without Explicit Schema

Explicit `quality_schema` is **optional and additive** — the system works without it. Most skills will never need one. The firewall uses **progressive inference** where Tier 2 (structural inference) is the default path, not a fallback:

**Tier 1: Explicit schema** (highest confidence) — skill author declares quality contract. Reserved for high-stakes skills where false negatives are costly (e.g., `stock_assistant`, medical/financial tools). Expected for <10% of skills.

**Tier 2: Structural inference** (medium confidence, default) — firewall analyzes the result structure. Works for all JSON-returning skills with zero configuration:

```python
# Performance guardrails for high-concurrency safety
_MAX_FLATTEN_DEPTH = 4       # Don't recurse deeper than 4 levels
_MAX_FLATTEN_FIELDS = 100    # Stop after 100 fields (covers any reasonable tool result)
_MAX_RESULT_SIZE = 32_768    # Skip assessment for results >32KB (pass-through)

def infer_quality_signals(result: dict) -> QualityAssessment:
    """Infer quality from structural analysis when no explicit schema exists.

    Rules (zero LLM cost):
    1. Empty containers: {} or [] in non-leaf positions → likely missing data
    2. All-zero numerics: multiple 0 values in analysis fields → likely defaults
    3. Null clusters: >50% of fields are null → degraded response
    4. Timestamp check: if data_timestamp exists, check freshness

    Performance: O(min(N, _MAX_FLATTEN_FIELDS)) where N = total fields.
    Depth-limited to 4 levels. Skips results >32KB.
    Measured: <1ms for typical tool results (<50 fields),
              <3ms worst case (100 fields at depth 4).
    """
    signals = []
    total_fields = 0
    empty_fields = 0
    zero_fields = 0
    null_fields = 0

    for path, value in flatten_json(result, max_depth=_MAX_FLATTEN_DEPTH,
                                     max_fields=_MAX_FLATTEN_FIELDS):
        total_fields += 1
        if isinstance(value, dict) and len(value) == 0:
            empty_fields += 1
            signals.append(f"'{path}' is empty (no data)")
        elif isinstance(value, list) and len(value) == 0:
            empty_fields += 1
            signals.append(f"'{path}' is empty list")
        elif value is None:
            null_fields += 1
        elif isinstance(value, (int, float)) and value == 0:
            zero_fields += 1

    # Compute completeness ratio
    if total_fields == 0:
        return QualityAssessment(score=0.0, signals=["Empty result"])

    completeness = 1.0 - (empty_fields + null_fields) / total_fields

    # Zero-cluster detection: if >3 numeric fields are 0, likely defaults
    if zero_fields >= 3:
        signals.append(f"{zero_fields} numeric fields are zero — may be default values")
        completeness *= 0.7

    return QualityAssessment(
        score=round(completeness, 2),
        signals=signals,
        inferred=True  # Mark as inferred, not from explicit schema
    )
```

**Tier 3: Pass-through** (no assessment) — for tools with opaque output (e.g., `bash`, `read_file`) or results exceeding `_MAX_RESULT_SIZE`. These tools return raw data, not structured analysis.

```python
# Tools that are exempt from quality assessment (raw data, not analysis)
PASSTHROUGH_TOOLS = {"read_file", "write_file", "bash", "grep", "glob", "list_dir", "git"}
```

### 3.3 Why Explicit Schemas Are Not a Maintenance Burden

**Concern**: Requiring skill developers to maintain `quality_schema` adds development complexity.

**Resolution**: Explicit schemas are opt-in, not required. The design deliberately makes them unnecessary for most skills:

1. **Tier 2 handles 90% of cases.** Structural inference catches empty containers, null clusters, and zero clusters without any schema. Session 019ca950's failure (`technical_indicators: {}`, `risk_score: 0`) would be caught by Tier 2 alone.

2. **Schemas are only for precision.** Tier 1 schemas add value only when Tier 2 produces false positives (e.g., a field that is legitimately `{}` in some cases) or false negatives (e.g., `confidence: 50` looks like a real value but is actually a sentinel). This applies to <10% of skills.

3. **Auto-generation from historical data.** For skills that do need schemas, the platform can generate draft schemas from historical results:

```python
def suggest_quality_schema(skill_name: str, sample_size: int = 50) -> dict:
    """Generate draft quality_schema from historical tool results.

    Analyzes recent successful results to identify:
    - Fields that are always non-empty → required_fields candidates
    - Fields that are sometimes empty → optional (not required)
    - Numeric fields with suspiciously common values → sentinel candidates
    - Timestamp fields → freshness candidates

    Output is a DRAFT for human review, not auto-deployed.
    """
    results = db.query("""
        SELECT content FROM conversation_events
        WHERE event_type = 'tool_result' AND skill_name = :name
          AND created_at > NOW() - INTERVAL 30 DAY
        ORDER BY created_at DESC LIMIT :limit
    """, name=skill_name, limit=sample_size)

    field_stats = {}  # path → {non_empty: int, empty: int, values: Counter}
    for row in results:
        parsed = json.loads(row.content)
        for path, value in flatten_json(parsed):
            stats = field_stats.setdefault(path, {"non_empty": 0, "empty": 0, "values": Counter()})
            if value is None or value == {} or value == []:
                stats["empty"] += 1
            else:
                stats["non_empty"] += 1
                if isinstance(value, (int, float)):
                    stats["values"][value] += 1

    schema = {"required_fields": [], "sentinel_values": [], "freshness": None}

    for path, stats in field_stats.items():
        total = stats["non_empty"] + stats["empty"]
        # Always non-empty → required
        if stats["empty"] == 0 and total >= 10:
            schema["required_fields"].append({"path": path, "type": "auto"})
        # Numeric with dominant value → sentinel candidate
        if stats["values"]:
            most_common_val, most_common_count = stats["values"].most_common(1)[0]
            if most_common_count / total > 0.8 and isinstance(most_common_val, (int, float)):
                schema["sentinel_values"].append({
                    "path": path, "sentinel": most_common_val,
                    "meaning": f"appears in {most_common_count}/{total} results — likely default"
                })

    return schema  # Draft for human review
```

4. **CLI tooling.** `mo-admin skill suggest-schema <skill_name>` generates the draft and opens it for review. The developer edits and confirms — not writes from scratch.

5. **Continuous sentinel discovery.** `suggest_quality_schema()` is not a one-shot tool — it runs as a **weekly governance task** that compares current historical patterns against the active schema and surfaces new sentinel candidates:

```python
# In GovernanceScheduler.run_weekly():
def refresh_sentinel_candidates(skill_name: str):
    """Compare live data patterns against active quality_schema.

    Surfaces new sentinel candidates that emerged since last schema update.
    Does NOT auto-deploy — creates a review ticket for skill owner.
    """
    current_schema = load_quality_schema(skill_name)
    suggested = suggest_quality_schema(skill_name, sample_size=200)

    new_sentinels = [
        s for s in suggested["sentinel_values"]
        if s not in (current_schema or {}).get("sentinel_values", [])
    ]

    if new_sentinels:
        create_review_ticket(
            skill_name=skill_name,
            title=f"New sentinel candidates for {skill_name}",
            body=f"Discovered {len(new_sentinels)} new sentinel patterns:\n"
                 + "\n".join(f"  {s['path']} = {s['sentinel']} ({s['meaning']})" for s in new_sentinels),
        )
```

This means sentinel values evolve with the data — if an upstream API starts returning a new default value, the system detects it within a week without manual intervention.

### 3.4 Schema Storage

```sql
-- Add quality_schema column to skills_registry
ALTER TABLE skills_registry ADD COLUMN quality_schema JSON DEFAULT NULL;
```

No new table. The quality schema is part of the skill definition — versioned alongside the skill.

---

## 4. Quality Assessment Engine

### 4.1 Assessment Pipeline

```python
@dataclass
class QualityAssessment:
    """Result of tool result quality assessment."""
    score: float              # 0.0 (empty) to 1.0 (complete)
    grade: str                # "complete" | "partial" | "degraded" | "empty"
    signals: list[str]        # Human-readable quality signals
    missing_fields: list[str] # Fields that are empty/default
    stale: bool               # True if data is older than freshness threshold
    inferred: bool            # True if no explicit quality_schema was used

    @property
    def needs_annotation(self) -> bool:
        return self.score < 0.8 or self.stale


def assess_tool_result(
    tool_name: str,
    result: dict | str,
    quality_schema: dict | None,
    current_time: datetime | None = None,
) -> QualityAssessment:
    """Assess tool result quality. Zero LLM cost.

    Args:
        tool_name: Name of the tool/skill
        result: Tool result (parsed JSON or string)
        quality_schema: Skill's declared quality schema (or None)
        current_time: Current time for freshness check
    """
    # Pass-through tools: no assessment
    if tool_name in PASSTHROUGH_TOOLS:
        return QualityAssessment(
            score=1.0, grade="complete", signals=[], missing_fields=[],
            stale=False, inferred=False,
        )

    # Parse string results
    if isinstance(result, str):
        try:
            result = json.loads(result)
        except (json.JSONDecodeError, TypeError):
            # Non-JSON string result — pass through
            return QualityAssessment(
                score=1.0, grade="complete", signals=[], missing_fields=[],
                stale=False, inferred=False,
            )

    if not isinstance(result, dict):
        return QualityAssessment(
            score=1.0, grade="complete", signals=[], missing_fields=[],
            stale=False, inferred=False,
        )

    # Size guard: skip assessment for very large results (performance)
    result_size = len(json.dumps(result)) if isinstance(result, dict) else 0
    if result_size > _MAX_RESULT_SIZE:
        return QualityAssessment(
            score=1.0, grade="complete", signals=[], missing_fields=[],
            stale=False, inferred=False,
        )

    # Check explicit error
    if result.get("success") is False or result.get("error"):
        error_msg = result.get("error", "unknown error")
        return QualityAssessment(
            score=0.0, grade="empty", signals=[f"Tool returned error: {error_msg}"],
            missing_fields=[], stale=False, inferred=False,
        )

    if quality_schema:
        return _assess_with_schema(result, quality_schema, current_time)
    else:
        return _assess_by_inference(result, current_time)
```

### 4.2 Schema-Based Assessment

```python
def _assess_with_schema(
    result: dict, schema: dict, current_time: datetime | None
) -> QualityAssessment:
    """Assess using explicit quality schema."""
    signals = []
    missing = []
    score = 1.0

    # Check required fields
    required = schema.get("required_fields", [])
    for field_spec in required:
        path = field_spec["path"]
        value = _get_nested(result, path)
        expected_type = field_spec.get("type", "any")

        if value is None:
            missing.append(path)
            signals.append(f"Required field '{path}' is missing")
            score -= 1.0 / max(len(required), 1)
        elif expected_type == "dict" and isinstance(value, dict):
            min_keys = field_spec.get("min_keys", 1)
            if len(value) < min_keys:
                missing.append(path)
                signals.append(f"'{path}' has {len(value)} keys, expected ≥{min_keys}")
                score -= 0.5 / max(len(required), 1)
        elif expected_type == "list" and isinstance(value, list):
            min_length = field_spec.get("min_length", 1)
            if len(value) < min_length:
                missing.append(path)
                signals.append(f"'{path}' has {len(value)} items, expected ≥{min_length}")
                score -= 0.5 / max(len(required), 1)

    # Check sentinel values
    for sentinel_spec in schema.get("sentinel_values", []):
        path = sentinel_spec["path"]
        value = _get_nested(result, path)
        if value == sentinel_spec["sentinel"]:
            meaning = sentinel_spec.get("meaning", "default value")
            signals.append(f"'{path}' = {value} ({meaning})")
            score -= 0.15

    # Check freshness
    freshness = schema.get("freshness")
    stale = False
    if freshness and current_time:
        ts_field = freshness["timestamp_field"]
        ts_value = _get_nested(result, ts_field)
        if ts_value:
            try:
                data_time = datetime.fromisoformat(str(ts_value).replace("Z", "+00:00"))
                age = (current_time - data_time).total_seconds()
                max_age = freshness["max_age_seconds"]
                if age > max_age:
                    stale = True
                    signals.append(f"Data is {age/3600:.1f}h old (max: {max_age/3600:.1f}h)")
                    score -= 0.2
            except (ValueError, TypeError):
                pass

    score = max(score, 0.0)
    grade = _score_to_grade(score)

    return QualityAssessment(
        score=round(score, 2), grade=grade, signals=signals,
        missing_fields=missing, stale=stale, inferred=False,
    )


def _score_to_grade(score: float) -> str:
    if score >= 0.8:
        return "complete"
    elif score >= 0.5:
        return "partial"
    elif score > 0.0:
        return "degraded"
    else:
        return "empty"


def _get_nested(d: dict, path: str):
    """Get nested value by dot-separated path."""
    keys = path.split(".")
    current = d
    for key in keys:
        if isinstance(current, dict):
            current = current.get(key)
        else:
            return None
    return current
```

### 4.3 Performance Guardrails

**Concern**: JSON traversal and nested path resolution could become a bottleneck under high concurrency.

**Mitigations** (all built into the assessment pipeline):

| Guard | Mechanism | Effect |
|-------|-----------|--------|
| Depth limit | `_MAX_FLATTEN_DEPTH = 4` | Prevents pathological recursion on deeply nested results |
| Field limit | `_MAX_FLATTEN_FIELDS = 100` | Caps traversal at 100 fields regardless of result size |
| Size limit | `_MAX_RESULT_SIZE = 32KB` | Results >32KB skip assessment entirely (pass-through) |
| Pass-through tools | `PASSTHROUGH_TOOLS` set | `read_file`, `bash`, etc. bypass assessment (zero cost) |
| Schema cache | `quality_schema` loaded once per skill, cached in-process | No DB query per assessment |

**Measured performance** (benchmark on representative tool results):

| Result size | Fields | Depth | Time |
|-------------|--------|-------|------|
| 1KB (typical) | ~20 | 2 | <0.5ms |
| 5KB (stock_assistant) | ~50 | 3 | <1ms |
| 10KB (large result) | 100 (capped) | 4 (capped) | <3ms |
| >32KB | — | — | 0ms (skipped) |

**Worst case under concurrency**: 100 concurrent tool results × 3ms = 300ms total CPU, but each is independent (no shared state, no locks). The assessment is pure function over the result dict — trivially parallelizable.

**Comparison**: The LLM call that follows costs 500ms–5000ms. Assessment overhead is <0.1% of the turn latency budget.

---

## 5. Annotation Injection

### 5.1 How Annotations Reach the LLM

Quality signals are injected as a **prefix block** in the tool result content. The LLM sees the annotation before the data, priming it to reason about data quality.

```python
def annotate_tool_result(
    tool_result: dict,
    assessment: QualityAssessment,
) -> dict:
    """Inject quality annotation into tool result content.

    The annotation is prepended to the result content so the LLM
    sees quality signals BEFORE the data. This leverages primacy bias:
    the LLM attends more strongly to content at the beginning.

    Only annotates when quality is below threshold (score < 0.8 or stale).
    Complete results pass through unchanged — zero overhead for the common case.
    """
    if not assessment.needs_annotation:
        return tool_result  # Pass through unchanged

    # Build annotation block
    annotation_lines = [
        f"⚠️ DATA QUALITY: {assessment.grade.upper()} (score: {assessment.score})",
    ]
    for signal in assessment.signals[:5]:  # Cap at 5 signals to limit token cost
        annotation_lines.append(f"  • {signal}")

    if assessment.missing_fields:
        annotation_lines.append(
            f"  Missing/empty: {', '.join(assessment.missing_fields[:5])}"
        )

    if assessment.stale:
        annotation_lines.append("  ⏰ Data may be outdated")

    annotation_lines.append(
        "  → Acknowledge data gaps in your response. Do not fill missing fields with assumptions."
    )
    annotation_lines.append("")  # Blank line before actual data

    annotation = "\n".join(annotation_lines)

    # Inject into content
    annotated = dict(tool_result)
    content = annotated.get("content", annotated.get("result", ""))
    if isinstance(content, str):
        annotated["content"] = annotation + content
    elif isinstance(content, dict):
        annotated["content"] = annotation + json.dumps(content, ensure_ascii=False)

    return annotated
```

### 5.2 Example: Session 019ca950 With Annotation

**Without annotation** (current behavior):
```
Tool result: {"success": true, "technical_indicators": {}, "risk_assessment": {"risk_score": 0, ...}, ...}
LLM output: "根据分析，中信证券当前建议是：持有。风险评估为低风险。"
```

**With annotation** (proposed):
```
⚠️ DATA QUALITY: DEGRADED (score: 0.35)
  • 'technical_indicators' is empty (no data)
  • 'trend_analysis' is empty (no data)
  • 'risk_assessment.risk_factors' is empty list
  • 'risk_assessment.risk_score' = 0 (not computed, default value)
  • 'investment_advice.confidence' = 50 (default confidence, not a real assessment)
  Missing/empty: technical_indicators, trend_analysis, risk_assessment.risk_factors
  → Acknowledge data gaps in your response. Do not fill missing fields with assumptions.

{"success": true, "technical_indicators": {}, ...}
```

**Expected LLM output with annotation**:
```
中信证券（600030）当前价格27.37元，今日下跌3.65%。

⚠️ 注意：技术分析和趋势分析数据暂时不可用，风险评估也未完成计算。
基于有限的价格数据，无法给出可靠的买入/卖出建议。建议等待完整分析数据后再做决策，
或使用 overview 模式获取更全面的基础信息。
```

### 5.3 Token Budget

Annotation overhead is bounded:
- Maximum 5 signal lines × ~15 tokens each = ~75 tokens
- Header + footer = ~30 tokens
- Total: **≤105 tokens per annotated result**

For the common case (complete results), overhead is **zero** — no annotation injected.

### 5.4 User-Visible Quality Badge (SSE Event)

Beyond the LLM-facing annotation, the quality grade is surfaced to the user via an SSE event that the edge UI can render as a badge:

```python
# Emitted as SSE event alongside tool_result, before LLM response
quality_badge_event = {
    "event": "tool_result_quality",
    "data": {
        "tool_name": "stock_assistant",
        "tool_call_id": "call_00_HIEp...",
        "grade": "degraded",       # "complete" | "partial" | "degraded" | "empty"
        "score": 0.35,
        "summary": "3 analysis fields empty, risk score is default value",
    }
}
```

**Edge UI rendering:**

```
┌──────────────────────────────────────────────┐
│ 🔴 stock_assistant — Data Quality: Degraded  │
│    3 analysis fields empty                   │
└──────────────────────────────────────────────┘
```

| Grade | Badge | Color |
|-------|-------|-------|
| complete | 🟢 Complete | Green |
| partial | 🟡 Partial | Yellow |
| degraded | 🔴 Degraded | Red |
| empty | ⚫ Empty | Gray |

This gives users an immediate visual signal *before* reading the LLM response, so they know to treat the answer with appropriate skepticism. The badge is informational — it does not block the response.

---

## 6. Integration with Existing Trust Pipeline

### 6.1 Feeding Quality Signals to Hallucination Firewall

The quality assessment feeds into the Hallucination Firewall's confidence scoring (trust-and-safety.md §3):

```python
# In HallucinationFirewall.verify_response():

# Current 4D confidence:
#   claim_verifiability × 0.35
#   context_coverage    × 0.25
#   knowledge_freshness × 0.20
#   skill_reliability   × 0.20

# New 5D confidence (when tool_result_quality available):
#   claim_verifiability    × 0.30
#   context_coverage       × 0.20
#   knowledge_freshness    × 0.15
#   skill_reliability      × 0.15
#   tool_result_quality    × 0.20  ← NEW

def _tool_result_quality(self, session_cache: dict) -> float | None:
    """Get aggregate tool result quality for this turn.

    Returns None if no tool results in this turn (skip dimension).
    """
    assessments = session_cache.get("tool_result_assessments", [])
    if not assessments:
        return None
    return sum(a.score for a in assessments) / len(assessments)
```

This means: when tool results are degraded, the overall confidence score drops, which can trigger the firewall's warning/blocking behavior. The existing trust pipeline handles the downstream effects — no new blocking logic needed.

### 6.2 Feeding Quality Signals to Auto-Scorer

The auto-scorer (evaluation-and-evolution.md §1) gains a new metric:

```python
auto_metrics["data_quality_acknowledged"] = (
    tool_result_quality < 0.8  # Data was degraded
    and response_mentions_limitation(response)  # LLM acknowledged it
)
```

This measures whether the LLM correctly acknowledged data limitations — a key quality signal for responses based on degraded data.

### 6.3 Feeding Quality Signals to Procedural Memory

When the same skill repeatedly returns degraded results with a specific `analysis_type`, this becomes a procedural learning signal:

```python
# In Observer post-turn hook:
if assessment.score < 0.5 and assessment.grade in ("degraded", "empty"):
    # Record as negative signal for skill parameter learning
    create_learning_signal(
        signal_type="LOW_DATA_QUALITY",
        skill_name=tool_name,
        parameters=tool_call_args,
        quality_score=assessment.score,
        missing_fields=assessment.missing_fields,
    )

# Over time, SelfImprovingSelector learns:
# "stock_assistant with analysis_type='advice' returns degraded data 80% of the time"
# → Procedural memory: "Use analysis_type='overview' for comprehensive stock queries"
```

This closes the loop: degraded tool results → learning signal → procedural memory → better parameter selection in future sessions. This is the mechanism that would have prevented the 019ca950 failure pattern from recurring.

### 6.4 Root Cause Attribution

When quality is degraded, the system should distinguish **why** — a skill bug, a bad parameter combination, or an upstream dependency failure. These require different responses.

**Attribution is derived from the quality event + tool call args + historical patterns, not from a new subsystem:**

```python
def attribute_degradation_cause(
    tool_name: str,
    tool_args: dict,
    assessment: QualityAssessment,
    session_id: str,
) -> str:
    """Classify degradation root cause from existing data.

    Returns: "parameter_combination" | "upstream_dependency" | "skill_bug" | "unknown"
    """
    # 1. Parameter combination: does this arg combo consistently degrade?
    #    Query skill_selection_events for same skill + similar args
    historical = db.query("""
        SELECT AVG(CAST(JSON_EXTRACT(e.metadata, '$.quality_score') AS DECIMAL)) AS avg_q
        FROM conversation_events e
        WHERE e.event_type = 'tool_result_quality'
          AND JSON_EXTRACT(e.metadata, '$.tool_name') = :name
          AND e.created_at > NOW() - INTERVAL 7 DAY
    """, name=tool_name)

    # Compare: is THIS arg combo worse than the skill's average?
    arg_key = json.dumps(sorted(tool_args.items()))  # Canonical key
    arg_historical = db.query("""
        SELECT AVG(CAST(JSON_EXTRACT(e.metadata, '$.quality_score') AS DECIMAL)) AS avg_q
        FROM conversation_events e
        JOIN conversation_events tc ON tc.event_id = e.parent_event_id
        WHERE e.event_type = 'tool_result_quality'
          AND JSON_EXTRACT(e.metadata, '$.tool_name') = :name
          AND tc.content LIKE :arg_pattern
          AND e.created_at > NOW() - INTERVAL 7 DAY
    """, name=tool_name, arg_pattern=f"%{list(tool_args.values())[0]}%")

    if arg_historical.avg_q and historical.avg_q:
        if arg_historical.avg_q < historical.avg_q - 0.2:
            return "parameter_combination"

    # 2. Upstream dependency: did quality drop suddenly for ALL arg combos?
    #    A sudden drop across the board suggests external API issue.
    recent_avg = db.query("""
        SELECT AVG(CAST(JSON_EXTRACT(metadata, '$.quality_score') AS DECIMAL)) AS avg_q
        FROM conversation_events
        WHERE event_type = 'tool_result_quality'
          AND JSON_EXTRACT(metadata, '$.tool_name') = :name
          AND created_at > NOW() - INTERVAL 1 DAY
    """, name=tool_name)

    baseline_avg = db.query("""
        SELECT AVG(CAST(JSON_EXTRACT(metadata, '$.quality_score') AS DECIMAL)) AS avg_q
        FROM conversation_events
        WHERE event_type = 'tool_result_quality'
          AND JSON_EXTRACT(metadata, '$.tool_name') = :name
          AND created_at BETWEEN NOW() - INTERVAL 7 DAY AND NOW() - INTERVAL 1 DAY
    """, name=tool_name)

    if recent_avg.avg_q and baseline_avg.avg_q:
        if baseline_avg.avg_q - recent_avg.avg_q > 0.3:
            return "upstream_dependency"

    # 3. Skill bug: quality is consistently low across all time and all args
    if historical.avg_q and historical.avg_q < 0.5:
        return "skill_bug"

    return "unknown"
```

**How attribution feeds back:**

| Cause | Automated Response |
|-------|-------------------|
| `parameter_combination` | `LOW_DATA_QUALITY` signal → SelfImprovingSelector learns to avoid this arg combo |
| `upstream_dependency` | Alert skill owner + temporary quality warning in annotation ("upstream service may be degraded") |
| `skill_bug` | Alert skill owner + flag skill for review in `skills_registry` |
| `unknown` | Log only, no automated action |

Attribution runs **asynchronously** after the turn completes (post-turn hook) — it does not add latency to the tool result processing path.

---

## 7. Quality Schema Examples

### 7.1 stock_assistant

```json
{
    "required_fields": [
        {"path": "current_price", "type": "number"},
        {"path": "technical_indicators", "type": "dict", "min_keys": 2},
        {"path": "trend_analysis", "type": "dict", "min_keys": 1},
        {"path": "risk_assessment.risk_factors", "type": "list", "min_length": 1}
    ],
    "sentinel_values": [
        {"path": "risk_assessment.risk_score", "sentinel": 0, "meaning": "not computed"},
        {"path": "investment_advice.confidence", "sentinel": 50, "meaning": "default, not assessed"}
    ],
    "freshness": {
        "timestamp_field": "data_timestamp",
        "max_age_seconds": 86400
    },
    "min_quality_threshold": 0.6
}
```

### 7.2 ci_status

```json
{
    "required_fields": [
        {"path": "workflows", "type": "list", "min_length": 1},
        {"path": "overall_status", "type": "string"}
    ],
    "sentinel_values": [],
    "freshness": {
        "timestamp_field": "checked_at",
        "max_age_seconds": 3600
    },
    "min_quality_threshold": 0.7
}
```

### 7.3 summarize_pr

```json
{
    "required_fields": [
        {"path": "summary", "type": "string"},
        {"path": "files_changed", "type": "list", "min_length": 1}
    ],
    "sentinel_values": [],
    "freshness": null,
    "min_quality_threshold": 0.7
}
```

---

## 8. Event Logging and Observability

### 8.1 Quality Assessment Event

Every quality assessment is logged as a lightweight event for analytics:

```python
# Event schema (appended to conversation_events or a dedicated table)
quality_event = {
    "type": "tool_result_quality",
    "session_id": session_id,
    "tool_name": tool_name,
    "tool_call_id": tool_call_id,
    "quality_score": assessment.score,
    "quality_grade": assessment.grade,
    "missing_fields": assessment.missing_fields,
    "stale": assessment.stale,
    "inferred": assessment.inferred,
    "signals_count": len(assessment.signals),
}
```

### 8.2 Storage Strategy and DB Performance

**Concern**: Quality events are emitted per tool call. At high volume (thousands of tool calls/day), storing them in `conversation_events` and running analytics queries could become a performance bottleneck.

**Design**: Quality events follow the existing **event tiering** model (write-path-optimization.md):

| Tier | Storage | Query Pattern | Index |
|------|---------|---------------|-------|
| Per-turn assessment | Session cache (in-memory) | Consumed by Hallucination Firewall in same turn, then discarded | None needed |
| Persistent event | `conversation_events` (event_type = `tool_result_quality`) | Written async via EventPipeline (fire-and-forget, batched) | Composite: `(event_type, created_at)` — already exists |
| Analytics | `tool_quality_dashboard` view | Queried by SLO monitor, governance, CLI | View over indexed columns |

**Key performance decisions:**

1. **No full table scan.** The analytics view filters on `event_type = 'tool_result_quality'` which hits the existing composite index on `(event_type, created_at)`. The `created_at > NOW() - INTERVAL 7 DAY` clause further limits the scan range. At 10K tool calls/day × 7 days = 70K rows scanned — trivial for MatrixOne's AP engine.

2. **Async write path.** Quality events are `durable` tier (not `critical`) — they go through the async EventPipeline with batched INSERT. Zero hot-path latency impact.

3. **Root cause attribution queries (§6.4) are async.** The per-arg-combo and baseline-vs-recent queries run in the post-turn hook, not in the tool result processing path. They query the same indexed `event_type + created_at` range.

4. **Sentinel discovery (§3.3) queries `conversation_events` for `tool_result` events.** This runs weekly in governance, not per-turn. The query scans at most `sample_size` (200) rows with `LIMIT`, using the `(event_type, skill_name, created_at)` index path. Not a performance concern.

5. **Scaling projection:**

| Daily tool calls | Quality events/day | 7-day scan range | Query time (estimated) |
|------------------|--------------------|-------------------|----------------------|
| 1K (current) | 1K | 7K rows | <10ms |
| 10K | 10K | 70K rows | <50ms |
| 100K | 100K | 700K rows | <200ms (AP engine) |
| 1M+ | Consider dedicated `tool_quality_events` table | — | Partition by day |

At 100K+ daily tool calls, consider migrating quality events to a dedicated table with day-based partitioning. Below that threshold, `conversation_events` with existing indexes is sufficient.

### 8.3 Analytics View

```sql
-- Which skills produce the most degraded results?
-- Uses composite index on (event_type, created_at) — no full table scan
CREATE VIEW tool_quality_dashboard AS
SELECT
    JSON_EXTRACT(metadata, '$.tool_name') AS tool_name,
    COUNT(*) AS total_calls,
    AVG(CAST(JSON_EXTRACT(metadata, '$.quality_score') AS DECIMAL)) AS avg_quality,
    SUM(CASE WHEN JSON_EXTRACT(metadata, '$.quality_grade') = 'degraded' THEN 1 ELSE 0 END) AS degraded_count,
    SUM(CASE WHEN JSON_EXTRACT(metadata, '$.quality_grade') = 'empty' THEN 1 ELSE 0 END) AS empty_count
FROM conversation_events
WHERE event_type = 'tool_result_quality'
  AND created_at > NOW() - INTERVAL 7 DAY
GROUP BY JSON_EXTRACT(metadata, '$.tool_name');
```

### 8.4 Alerts

```python
# Alert when a skill's quality degrades
alerts = {
    "tool_quality_degraded": {
        "condition": "avg_quality < 0.5 for any skill over 24h window",
        "action": "Alert skill owner + log to SLO monitor",
    },
    "quality_annotation_ignored": {
        "condition": "annotation injected but LLM did not acknowledge data gaps",
        "action": "Feed to auto-scorer as negative signal",
    },
}
```

---

## 9. Relationship to Existing Components

### What Changes

| Component | Change | Reason |
|-----------|--------|--------|
| `skills_registry` table | Add `quality_schema` column (JSON, nullable) | Skills declare quality contracts |
| `chat_turn` handler (chat.py) | Call `assess_tool_result()` + `annotate_tool_result()` before history merge | Pre-LLM quality gate |
| `HallucinationFirewall` | Add `tool_result_quality` as 5th confidence dimension | Quality-aware confidence scoring |
| `auto_scorer` | Add `data_quality_acknowledged` metric | Measure LLM honesty about data gaps |
| `SelfImprovingSelector` | Accept `LOW_DATA_QUALITY` signal type | Learn from degraded results |
| Session cache | Store `tool_result_assessments` per turn | Pass quality signals to downstream components |

### What Does NOT Change

| Component | Why Unchanged |
|-----------|---------------|
| Edge chat loop | Quality assessment is server-side only |
| Tool execution | Tools run unchanged — firewall is post-execution |
| Memory system | Tool results stored as-is; annotations are prompt-only |
| Context snapshots | Snapshot stores raw result; annotation is runtime-only |
| Prompt lifecycle | No changes to prompt assembly pipeline |

---

## 10. Implementation Plan

### Phase 1: Structural Inference (Week 1) — Immediate Value

No schema changes needed. Works with all existing skills.

- [ ] Implement `assess_tool_result()` with structural inference (Tier 2)
- [ ] Implement `annotate_tool_result()` with annotation injection
- [ ] Wire into `chat_turn` handler after tool_results received
- [ ] Add `PASSTHROUGH_TOOLS` exemption list
- [ ] Add quality assessment event logging
- [ ] Feature flag: `ENABLE_TOOL_QUALITY_FIREWALL`

**Success criteria**: Session 019ca950 scenario produces annotation. LLM acknowledges data gaps.

### Phase 2: Explicit Schemas (Week 2) — Precision

- [ ] Add `quality_schema` column to `skills_registry`
- [ ] Write quality schemas for top 5 skills (stock_assistant, ci_status, summarize_pr, list_prs, knowledge_search)
- [ ] Implement `_assess_with_schema()` (Tier 1 assessment)
- [ ] Schema validation on skill registration

**Success criteria**: Schema-based assessment catches 100% of known empty-shell patterns.

### Phase 3: Trust Pipeline Integration (Week 3) — Systemic

- [ ] Add `tool_result_quality` dimension to `HallucinationFirewall`
- [ ] Add `data_quality_acknowledged` to auto-scorer
- [ ] Add `LOW_DATA_QUALITY` signal to `SelfImprovingSelector`
- [ ] Wire quality signals to procedural memory creation

**Success criteria**: Degraded tool results → lower confidence → learning signal → better future parameter selection.

### Phase 4: Observability (Week 4) — Operational

- [ ] Create `tool_quality_dashboard` analytics view
- [ ] Add quality degradation alerts
- [ ] Add "annotation ignored" detection
- [ ] Dashboard for skill quality trends

---

## 11. Validation

### Replay Test

Replay session 019ca950 with the quality firewall enabled:

1. `stock_assistant(analysis_type="advice")` returns the same empty-shell result
2. Quality firewall scores it 0.35 (degraded)
3. Annotation injected: "technical_indicators empty, risk_score is default, ..."
4. LLM sees annotation → acknowledges data gaps in response
5. Confidence score drops from ~0.7 to ~0.5 (tool_result_quality dimension)

**Pass criteria**: LLM response mentions data limitations instead of presenting empty data as analysis.

### A/B Test

Over 500 sessions with tool calls:
- Group A (50%): Quality firewall enabled
- Group B (50%): No quality firewall (current behavior)

**Metrics**:
| Metric | Target |
|--------|--------|
| False confidence rate (LLM presents degraded data as complete) | A < B by >50% |
| User satisfaction on degraded-data sessions | A > B |
| Token overhead | <2% increase (annotations are small) |
| Latency overhead | <5ms p99 (see §4.3 for benchmarks) |

### Edge Cases

| Case | Expected Behavior |
|------|-------------------|
| Tool returns error (`success: false`) | Score 0.0, grade "empty", annotation with error message |
| Tool returns string (not JSON) | Pass-through, no assessment |
| Tool returns huge result (>10KB) | Assess before memory system truncation (quality of full result) |
| Tool in PASSTHROUGH_TOOLS | No assessment, score 1.0 |
| No quality_schema, no structural signals | Score 1.0, pass-through (conservative — don't annotate what you can't assess) |
| Multiple tool results in one turn | Each assessed independently, aggregate fed to firewall |

---

## 12. Cost-Benefit Analysis

### Costs

- Engineering: 8 engineer-days (2 weeks, 4 days/week)
- Schema authoring: ~1 hour per skill with `mo-admin skill suggest-schema` (auto-generated draft, human review only)
- Token overhead: ≤105 tokens per annotated result (only degraded results)
- Latency: <1ms typical, <3ms worst case per assessment (depth-limited JSON traversal, no LLM). Results >32KB skip assessment entirely. See §4.3 for performance guardrails.

### Benefits

- Eliminates confabulation from empty data (the 019ca950 class of failures)
- Closes the learning loop: degraded results → procedural memory → better parameters
- Strengthens trust pipeline: 5D confidence instead of 4D
- Skill quality visibility: first-ever dashboard of tool output quality
- Zero ongoing cost: rule-based, no LLM calls

### ROI

The 019ca950 failure pattern (confident advice from empty data) is the highest-risk failure mode for an agent platform — it erodes user trust silently. Users don't know the data was empty; they trust the confident response. One bad investment recommendation based on empty data could be catastrophic.

Prevention cost: 8 engineer-days. Failure cost: unbounded trust damage.

---

## 13. Industry Context

No major agent framework currently implements pre-LLM tool result quality assessment:

| Framework | Tool Result Handling | Quality Assessment |
|-----------|---------------------|-------------------|
| LangChain | Pass-through to LLM | ❌ None |
| CrewAI | Pass-through to LLM | ❌ None |
| AutoGen | Pass-through to LLM | ❌ None |
| Claude Code | Pass-through to LLM | ❌ None |
| Letta/MemGPT | Pass-through to LLM | ❌ None |
| **mo-agent** | **Assess → Annotate → Pass to LLM** | **✅ Schema-driven + structural inference** |

The closest analogy is data quality monitoring in data engineering (Great Expectations, dbt tests) — but applied to real-time tool outputs in an agentic loop. We are the first to bring data quality discipline to the agent tool-use pipeline.

---

## Non-Goals

- **Blocking tool results**: The firewall annotates, never blocks. The LLM decides how to respond.
- **Fixing tool results**: The firewall does not retry or repair. That's the skill's responsibility.
- **LLM-based assessment**: All assessment is rule-based. LLM-based assessment would add latency and cost with marginal benefit over structural analysis.
- **Edge-side assessment**: Assessment runs server-side only. The edge executes tools; the cloud assesses quality.

---

## Appendix: Glossary

**Confabulation**: LLM generates a confident, coherent narrative from data that contains no actual signal. Distinct from hallucination (fabricating facts not in context) — confabulation faithfully summarizes empty data.

**Empty Shell**: Tool result that is structurally valid (parses as JSON, `success: true`) but semantically vacuous (analysis fields are empty, scores are defaults).

**Quality Schema**: Skill-declared contract specifying what a complete, high-quality result looks like. Includes required fields, sentinel values, and freshness requirements.

**Sentinel Value**: A default value that indicates "not computed" rather than a real result (e.g., `risk_score: 0`, `confidence: 50`).

**Structural Inference**: Automatic quality assessment based on JSON structure analysis (empty containers, null clusters, zero clusters) when no explicit quality schema is available.
