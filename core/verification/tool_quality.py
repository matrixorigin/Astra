"""Tool Result Quality Firewall — pre-LLM gate that detects vacuous tool results.

Assesses structural quality of tool results *before* they enter the LLM context
window, so the model can respond honestly instead of confabulating from empty data.

Three-tier assessment:
  Tier 1: Explicit quality_schema (future — skill declares expected fields)
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

PASSTHROUGH_TOOLS: frozenset[str] = frozenset({
    "read_file", "write_file", "bash", "grep", "glob", "list_dir", "git",
    "get_agent_info", "reflect",
})

_MAX_DEPTH = 4
_MAX_FIELDS = 100
_MAX_RESULT_SIZE = 32_768  # 32 KB

_STALE_SECONDS = 86_400  # 24 h


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
    """Yield (dotted_path, leaf_value) pairs, depth- and field-limited."""
    count = 0
    stack: list[tuple[str, Any, int]] = [("", d, 0)]
    while stack:
        prefix, obj, depth = stack.pop()
        if isinstance(obj, dict) and depth < max_depth:
            for k, v in obj.items():
                path = f"{prefix}.{k}" if prefix else k
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

    # Size guard
    try:
        if sys.getsizeof(json.dumps(data)) > _MAX_RESULT_SIZE:
            return QualityAssessment(tool_name=tool_name, score=1.0, grade="complete")
    except (TypeError, ValueError):
        pass

    # Explicit error
    if data.get("success") is False or (
        "error" in data and data["error"] and not data.get("success")
    ):
        return QualityAssessment(
            tool_name=tool_name, score=0.0, grade="empty",
            signals=["explicit_error: tool returned error"],
        )

    # Structural inference (Tier 2)
    signals: list[str] = []
    leaves = list(flatten_json(data))
    total = len(leaves)

    if total == 0:
        return QualityAssessment(
            tool_name=tool_name, score=0.0, grade="empty",
            signals=["all_empty: result has no leaf values"],
        )

    # Empty containers
    empty_count = sum(
        1 for _, v in leaves
        if v is None or v == {} or v == [] or v == ""
    )

    # Zero cluster: numeric fields that are exactly 0
    zero_count = sum(
        1 for _, v in leaves
        if isinstance(v, (int, float)) and v == 0
    )

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

    # Staleness check
    stale = False
    now = current_time or datetime.now(timezone.utc)
    for path, val in leaves:
        if "timestamp" in path.lower() or "date" in path.lower():
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
        tool_name=tool_name, score=round(score, 2), grade=grade,
        signals=signals[:5], stale=stale,
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
    lines.append("⚠ Respond honestly about data limitations. Do not present missing data as real analysis.")
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
