"""Tool Result Quality Firewall — pre-LLM gate that detects vacuous tool results.

Assesses structural quality of tool results *before* they enter the LLM context
window, so the model can respond honestly instead of confabulating from empty data.

Three-tier assessment:
  Tier 1: Explicit quality_schema (skill declares expected fields)
  Tier 2: Structural inference (default) — empty containers, null/zero clusters, staleness
  Tier 3: Pass-through for raw-data tools (file I/O, shell, etc.)
"""

from __future__ import annotations

import json
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Generator

from core.logging_config import get_logger

logger = get_logger(__name__)

# ── Constants ────────────────────────────────────────────────────────────────

# Tools where structural quality assessment is not meaningful:
# - File/shell tools return raw data (no expected schema)
# - get_agent_info / reflect return runtime metadata where zeros are
#   legitimate (e.g. new session has 0 events) — penalising them causes
#   the LLM to hallucinate "data quality issues" on perfectly valid data.
PASSTHROUGH_TOOLS: frozenset[str] = frozenset(
    {
        "read_file",
        "write_file",
        "bash",
        "grep",
        "glob",
        "list_dir",
        "git",
        "get_agent_info",
        "reflect",
        "introspection",
    }
)

_MAX_DEPTH = 4
_MAX_FIELDS = 100
_MAX_RESULT_SIZE = 32_768  # 32 KB

_STALE_SECONDS = 86_400  # 24 h

# Explicit timestamp field names to check for staleness
# Only exact matches (case-insensitive) to avoid false positives like "update_date_info"
_TIMESTAMP_FIELDS = frozenset(
    {
        "timestamp",
        "created_at",
        "updated_at",
        "date",
        "time",
        "last_updated",
        "modified_at",
        "published_at",
        "fetched_at",
    }
)


# ── Data structures ─────────────────────────────────────────────────────────


@dataclass
class QualityAssessment:
    tool_name: str
    score: float  # 0.0 – 1.0
    grade: str  # "complete" | "partial" | "degraded" | "empty"
    signals: list[str] = field(default_factory=list)
    stale: bool = False

    @property
    def needs_annotation(self) -> bool:
        return self.score < 0.8 or self.stale


# ── Helpers ──────────────────────────────────────────────────────────────────


def _score_to_grade(score: float) -> str:
    if score >= 0.8:
        return "complete"
    if score >= 0.5:
        return "partial"
    if score > 0.0:
        return "degraded"
    return "empty"


def flatten_json(
    d: dict[str, Any],
    *,
    max_depth: int = _MAX_DEPTH,
    max_fields: int = _MAX_FIELDS,
) -> Generator[tuple[str, Any], None, None]:
    """Yield (dotted_path, leaf_value) pairs, depth- and field-limited.

    Handles circular references by tracking visited object IDs.
    """
    count = 0
    seen: set[int] = set()
    stack: list[tuple[str, Any, int]] = [("", d, 0)]

    while stack:
        prefix, obj, depth = stack.pop()

        # Circular reference guard: skip already-visited objects
        if isinstance(obj, (dict, list)):
            obj_id = id(obj)
            if obj_id in seen:
                continue
            seen.add(obj_id)

        if isinstance(obj, dict) and depth < max_depth:
            for k, v in obj.items():
                path = f"{prefix}.{k}" if prefix else k
                stack.append((path, v, depth + 1))
        elif isinstance(obj, list) and depth < max_depth:
            for i, v in enumerate(obj):
                path = f"{prefix}[{i}]"
                stack.append((path, v, depth + 1))
        else:
            yield prefix, obj
            count += 1
            if count >= max_fields:
                return


# ── Core assessment ──────────────────────────────────────────────────────────


def assess_tool_result(
    tool_name: str,
    result: Any,
    *,
    current_time: datetime | None = None,
) -> QualityAssessment:
    """Assess structural quality of a tool result. Returns QualityAssessment."""

    # Tier 3: pass-through
    if tool_name in PASSTHROUGH_TOOLS:
        return QualityAssessment(tool_name=tool_name, score=1.0, grade="complete")

    # Parse string results
    data = result
    if isinstance(data, str):
        try:
            data = json.loads(data)
        except (json.JSONDecodeError, TypeError):
            return QualityAssessment(tool_name=tool_name, score=1.0, grade="complete")

    if not isinstance(data, dict):
        return QualityAssessment(tool_name=tool_name, score=1.0, grade="complete")

    # Size guard: skip assessment for very large results to avoid performance issues
    try:
        serialized = json.dumps(data)
        if len(serialized) > _MAX_RESULT_SIZE:
            return QualityAssessment(tool_name=tool_name, score=1.0, grade="complete")
    except (TypeError, ValueError):
        # Non-serializable data, pass through
        return QualityAssessment(tool_name=tool_name, score=1.0, grade="complete")

    # Explicit error
    if data.get("success") is False or (
        "error" in data and data["error"] and not data.get("success")
    ):
        return QualityAssessment(
            tool_name=tool_name,
            score=0.0,
            grade="empty",
            signals=["explicit_error: tool returned error"],
        )

    # Tier 1: schema-based assessment (if skill has quality_schema)
    schema = load_quality_schema(tool_name)
    if schema:
        return assess_with_schema(data, schema, tool_name, current_time=current_time)

    # Tier 2: structural inference (default)
    signals: list[str] = []
    leaves = list(flatten_json(data))
    total = len(leaves)

    if total == 0:
        return QualityAssessment(
            tool_name=tool_name,
            score=0.0,
            grade="empty",
            signals=["all_empty: result has no leaf values"],
        )

    # Empty containers
    empty_count = sum(1 for _, v in leaves if v is None or v == {} or v == [] or v == "")

    # Zero cluster: numeric fields that are exactly 0
    zero_count = sum(1 for _, v in leaves if isinstance(v, (int, float)) and v == 0)

    # Null cluster
    null_count = sum(1 for _, v in leaves if v is None)

    if empty_count == total:
        signals.append("all_empty: every field is empty/null/zero")
    else:
        if empty_count > 0:
            signals.append(f"empty_containers: {empty_count}/{total} fields empty")
        if null_count > total * 0.5:
            signals.append(f"null_cluster: {null_count}/{total} fields null")
        if zero_count >= 3:
            signals.append(f"zero_cluster: {zero_count} numeric fields are 0")

    # Staleness check: only check explicit timestamp fields to avoid false positives
    stale = False
    now = current_time or datetime.now(timezone.utc)
    for path, val in leaves:
        # Extract the final field name from dotted path (e.g., "data.timestamp" -> "timestamp")
        field_name = path.split(".")[-1].lower() if path else ""
        if field_name in _TIMESTAMP_FIELDS:
            if isinstance(val, str):
                try:
                    ts = datetime.fromisoformat(val.replace("Z", "+00:00"))
                    if (now - ts).total_seconds() > _STALE_SECONDS:
                        stale = True
                        signals.append(f"stale_data: {path} is >{_STALE_SECONDS // 3600}h old")
                except (ValueError, TypeError):
                    pass

    # Score: proportion of non-empty leaves
    non_empty = total - empty_count
    score = non_empty / total if total > 0 else 0.0

    # Zero cluster penalty: if ≥3 zeros and they dominate numeric fields
    numeric_count = sum(1 for _, v in leaves if isinstance(v, (int, float)))
    if zero_count >= 3 and numeric_count > 0 and zero_count / numeric_count > 0.5:
        score = min(score, 0.4)
        if not any("zero_cluster" in s for s in signals):
            signals.append(f"zero_cluster: {zero_count}/{numeric_count} numeric fields are 0")

    grade = _score_to_grade(score)
    return QualityAssessment(
        tool_name=tool_name,
        score=round(score, 2),
        grade=grade,
        signals=signals[:5],
        stale=stale,
    )


# ── Annotation ───────────────────────────────────────────────────────────────


def annotate_tool_result(
    tool_result: dict[str, Any],
    assessment: QualityAssessment,
) -> dict[str, Any]:
    """Prepend quality annotation to tool result so LLM sees data quality signals.

    Returns the (possibly modified) tool_result dict. Does NOT mutate if
    assessment.needs_annotation is False.
    """
    if not assessment.needs_annotation:
        return tool_result

    lines = [f"[TOOL QUALITY: {assessment.grade.upper()} — score {assessment.score}]"]
    for sig in assessment.signals[:5]:
        lines.append(f"  • {sig}")
    lines.append(
        "⚠ Respond honestly about data limitations. Do not present missing data as real analysis."
    )
    annotation = "\n".join(lines)

    # Annotate the content/result field
    out = dict(tool_result)
    for key in ("content", "result"):
        if key in out:
            original = out[key]
            if isinstance(original, str):
                out[key] = f"{annotation}\n---\n{original}"
            else:
                out[key] = f"{annotation}\n---\n{json.dumps(original)}"
            return out

    # Fallback: add as new field
    out["_quality_annotation"] = annotation
    return out


# ── Tier 1: Schema-based assessment ─────────────────────────────────────────


def _get_nested(d: dict[str, Any], path: str) -> Any:
    """Get nested value by dot-separated path."""
    current: Any = d
    for key in path.split("."):
        if isinstance(current, dict):
            current = current.get(key)
        else:
            return None
    return current


def assess_with_schema(
    result: dict[str, Any],
    schema: dict[str, Any],
    tool_name: str = "",
    *,
    current_time: datetime | None = None,
) -> QualityAssessment:
    """Tier 1: Assess using explicit quality_schema from skills_registry."""
    signals: list[str] = []
    score = 1.0

    # Required fields
    required = schema.get("required_fields", [])
    for spec in required:
        path = spec["path"]
        value = _get_nested(result, path)
        expected_type = spec.get("type", "any")

        if value is None:
            signals.append(f"missing: '{path}'")
            score -= 1.0 / max(len(required), 1)
        elif expected_type == "dict" and isinstance(value, dict):
            min_keys = spec.get("min_keys", 1)
            if len(value) < min_keys:
                signals.append(f"'{path}' has {len(value)} keys, need ≥{min_keys}")
                score -= 0.5 / max(len(required), 1)
        elif expected_type == "list" and isinstance(value, list):
            min_length = spec.get("min_length", 1)
            if len(value) < min_length:
                signals.append(f"'{path}' has {len(value)} items, need ≥{min_length}")
                score -= 0.5 / max(len(required), 1)

    # Sentinel values
    for spec in schema.get("sentinel_values", []):
        path = spec["path"]
        value = _get_nested(result, path)
        if value == spec["sentinel"]:
            meaning = spec.get("meaning", "default value")
            signals.append(f"sentinel: '{path}' = {value} ({meaning})")
            score -= 0.15

    # Freshness
    stale = False
    freshness = schema.get("freshness")
    now = current_time or datetime.now(timezone.utc)
    if freshness and freshness.get("timestamp_field"):
        ts_val = _get_nested(result, freshness["timestamp_field"])
        if isinstance(ts_val, str):
            try:
                ts = datetime.fromisoformat(ts_val.replace("Z", "+00:00"))
                age = (now - ts).total_seconds()
                max_age = freshness.get("max_age_seconds", _STALE_SECONDS)
                if age > max_age:
                    stale = True
                    signals.append(f"stale: {age / 3600:.1f}h old (max {max_age / 3600:.1f}h)")
                    score -= 0.2
            except (ValueError, TypeError):
                pass

    score = max(0.0, round(score, 2))
    return QualityAssessment(
        tool_name=tool_name,
        score=score,
        grade=_score_to_grade(score),
        signals=signals[:5],
        stale=stale,
    )


# ── Schema loader ────────────────────────────────────────────────────────────

from functools import lru_cache
from typing import Callable

# Injected schema loader — set by api layer at startup
_schema_loader: Callable[[str], dict[str, Any] | None] | None = None


def set_schema_loader(loader: Callable[[str], dict[str, Any] | None]) -> None:
    """Inject schema loader function. Called by api layer at startup."""
    global _schema_loader
    _schema_loader = loader


@lru_cache(maxsize=128)
def _cached_schema(tool_name: str, _cache_key: int) -> dict[str, Any] | None:
    """Internal cached loader. _cache_key rotates to invalidate cache."""
    if _schema_loader is None:
        return None
    return _schema_loader(tool_name)


_cache_generation: int = 0


def load_quality_schema(tool_name: str) -> dict[str, Any] | None:
    """Load quality_schema for a tool. Uses injected loader with LRU cache."""
    return _cached_schema(tool_name, _cache_generation)


def invalidate_schema_cache() -> None:
    """Invalidate schema cache (call after skill registration)."""
    global _cache_generation
    _cache_generation += 1


# ── Annotation-ignored detection (Phase 4) ───────────────────────────────────

_LIMITATION_KEYWORDS = frozenset(
    {
        "不完整",
        "数据缺失",
        "无法确认",
        "数据不足",
        "暂无数据",
        "incomplete",
        "missing data",
        "unavailable",
        "insufficient",
        "cannot confirm",
        "no data",
        "data limitation",
        "data quality",
    }
)


def response_acknowledges_limitation(response: str) -> bool:
    """Check if LLM response mentions data limitations.

    Used by auto-scorer to determine data_quality_acknowledged.
    """
    lower = response.lower()
    return any(kw in lower for kw in _LIMITATION_KEYWORDS)
