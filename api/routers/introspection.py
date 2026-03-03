"""Introspection API — cloud-side data for get_agent_info tool.

Design principle: all numeric reasoning happens in Python functions.
Callers (LLM) receive conclusions, not raw data.
"""

import json
from dataclasses import dataclass

from fastapi import APIRouter, Depends, HTTPException, Query
from sqlalchemy import text
from sqlalchemy.exc import SQLAlchemyError
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from core.logging_config import get_logger

logger = get_logger(__name__)
router = APIRouter()

# ---------------------------------------------------------------------------
# Thresholds — single source of truth for all analysis functions
# ---------------------------------------------------------------------------

ZONE_UTIL_HIGH = 0.80
ZONE_UTIL_MEDIUM = 0.60
RELEVANCE_HIGH = 0.70
RELEVANCE_LOW = 0.40
POLLUTION_THRESHOLD = 0.30
POLLUTION_STATUS_POLLUTED = 0.25
POLLUTION_STATUS_NOISY = 0.10
QUALITY_GOOD = 0.60
QUALITY_DEGRADED = 0.35
TREND_CHANGE_PCT = 0.10
COMPACTION_DROP_PCT = 0.80
COMPACTION_EFFECTIVE_PCT = 0.25
DEGRADATION_DELTA = 0.15
ZONE_BALANCE_TOLERANCE = 0.15
# Token-to-char ratio: conservative (handles CJK where 1 char ≈ 1-2 tokens)
TOKEN_CHAR_RATIO = 2

# Task-type → ideal zone weight distribution.
_IDEAL_ZONE_WEIGHTS: dict[str, dict[str, float]] = {
    "code_gen":  {"code": 0.40, "history": 0.30, "memory": 0.15},
    "qa":        {"history": 0.45, "memory": 0.30, "code": 0.10},
    "debugging": {"code": 0.45, "history": 0.25, "memory": 0.15},
}
_DEFAULT_WEIGHTS: dict[str, float] = {"history": 0.35, "code": 0.25, "memory": 0.20}


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

def _verify_session_owner(db: Session, session_id: str, user_id: str) -> None:
    """Raise 404 if session doesn't exist or doesn't belong to user."""
    row = db.execute(
        text("SELECT user_id FROM agent_sessions WHERE session_id = :sid"),
        {"sid": session_id},
    ).fetchone()
    if not row or row[0] != user_id:
        raise HTTPException(status_code=404, detail="Session not found")


def _compute_trend(token_history: list[int]) -> str:
    """Single trend computation used by all callers."""
    if len(token_history) < 2:
        return "stable"
    delta = token_history[0] - token_history[-1]
    pct = delta / max(token_history[-1], 1)
    if pct > TREND_CHANGE_PCT:
        return "growing"
    if pct < -TREND_CHANGE_PCT:
        return "shrinking"
    return "stable"


# ---------------------------------------------------------------------------
# Pure analysis functions (no DB access, fully testable)
# ---------------------------------------------------------------------------

def _analyze_context_health(budget: dict, total_tokens_history: list[int]) -> dict:
    """Compute zone utilization, bottleneck, trend, recommendation."""
    zones = []
    bottleneck_zone = None
    bottleneck_util = 0.0
    for zone, vals in budget.items():
        allocated = vals.get("allocated", 0)
        used = vals.get("used", 0)
        if allocated <= 0:
            continue
        util = round(used / allocated, 2)
        status = "high" if util >= ZONE_UTIL_HIGH else ("medium" if util >= ZONE_UTIL_MEDIUM else "ok")
        zones.append({"name": zone, "utilization": util, "status": status})
        if util > bottleneck_util:
            bottleneck_util = util
            bottleneck_zone = zone

    trend = _compute_trend(total_tokens_history)

    if bottleneck_util >= ZONE_UTIL_HIGH:
        recommendation = f"{bottleneck_zone} zone near limit — compaction recommended"
    elif trend == "growing" and len(total_tokens_history) >= 3:
        recommendation = "token usage growing — monitor for compaction trigger"
    else:
        recommendation = "context healthy"

    return {
        "zones": zones,
        "bottleneck": bottleneck_zone,
        "trend": trend,
        "recommendation": recommendation,
    }


def _zone_balance(budget: dict, task_type: str | None) -> dict:
    """Check if zone allocation matches the task type's ideal distribution."""
    managed_zones = {k: v for k, v in budget.items()
                     if k not in ("system", "skills", "reserve") and v.get("allocated", 0) > 0}
    if not managed_zones:
        return {"balanced": True, "misallocated_zone": None, "matched_profile": None}

    total_alloc = sum(v["allocated"] for v in managed_zones.values())
    actual = {k: v["allocated"] / total_alloc for k, v in managed_zones.items()}

    profile = task_type or ""
    ideal = _IDEAL_ZONE_WEIGHTS.get(profile, _DEFAULT_WEIGHTS)
    matched = profile if profile in _IDEAL_ZONE_WEIGHTS else "default"

    worst_zone = None
    worst_gap = 0.0
    for zone, ideal_pct in ideal.items():
        gap = abs(actual.get(zone, 0) - ideal_pct)
        if gap > worst_gap:
            worst_gap = gap
            worst_zone = zone

    balanced = worst_gap < ZONE_BALANCE_TOLERANCE
    return {
        "balanced": balanced,
        "misallocated_zone": worst_zone if not balanced else None,
        "matched_profile": matched,
        "recommendation": f"{worst_zone} zone allocation off by {worst_gap:.0%} for {matched} tasks" if not balanced else "zone balance ok",
    }


def _pollution_ratio(relevance_scores: dict) -> dict:
    """Fraction of selected events with low relevance — wasted context budget."""
    if not relevance_scores:
        return {"pollution_pct": 0.0, "status": "clean"}
    scores = list(relevance_scores.values())
    low = sum(1 for s in scores if s < POLLUTION_THRESHOLD)
    pct = round(low / len(scores), 2)
    status = "polluted" if pct > POLLUTION_STATUS_POLLUTED else ("noisy" if pct > POLLUTION_STATUS_NOISY else "clean")
    return {"pollution_pct": pct, "status": status,
            "recommendation": "re-retrieve or raise relevance threshold" if status == "polluted" else "ok"}


def _compaction_effectiveness(token_history: list[int]) -> dict:
    """Detect compaction events (sudden token drops) and measure their effect."""
    if len(token_history) < 2:
        return {"compactions_detected": 0}
    compactions = []
    for i in range(len(token_history) - 1):
        newer, older = token_history[i], token_history[i + 1]
        if older > 0 and newer < older * COMPACTION_DROP_PCT:
            reduction = round((older - newer) / older, 2)
            compactions.append({"turns_ago": i + 1, "reduction_pct": reduction})
    if not compactions:
        return {"compactions_detected": 0, "status": "none observed"}
    avg_reduction = round(sum(c["reduction_pct"] for c in compactions) / len(compactions), 2)
    return {
        "compactions_detected": len(compactions),
        "avg_reduction_pct": avg_reduction,
        "status": "effective" if avg_reduction >= COMPACTION_EFFECTIVE_PCT else "weak — consider more aggressive compaction",
    }


def _compaction_forecast(token_history: list[int], limit: int) -> dict:
    """Predict turns until compaction based on recent growth rate."""
    if len(token_history) < 2:
        return {"turns_remaining": None, "growth_rate_per_turn": None}
    growth = (token_history[0] - token_history[-1]) / (len(token_history) - 1)
    if growth <= 0:
        return {"turns_remaining": None, "growth_rate_per_turn": round(growth, 1)}
    remaining = max(0, limit - token_history[0])
    return {"turns_remaining": round(remaining / growth), "growth_rate_per_turn": round(growth, 1)}


def _relevance_quality(relevance_scores: dict) -> dict:
    """Summarise relevance score distribution — high/medium/low counts + mean."""
    if not relevance_scores:
        return {"mean": None, "high": 0, "medium": 0, "low": 0, "total": 0}
    scores = list(relevance_scores.values())
    mean = round(sum(scores) / len(scores), 3)
    high = sum(1 for s in scores if s >= RELEVANCE_HIGH)
    medium = sum(1 for s in scores if RELEVANCE_LOW <= s < RELEVANCE_HIGH)
    low = sum(1 for s in scores if s < RELEVANCE_LOW)
    quality = "good" if mean >= QUALITY_GOOD else ("degraded" if mean >= QUALITY_DEGRADED else "poor")
    return {"mean": mean, "high": high, "medium": medium, "low": low,
            "total": len(scores), "quality": quality}


@dataclass
class _SnapshotContentRow:
    """Named container for content columns — avoids fragile tuple indexing."""
    selected_events: str | None
    code_context: str | None
    skill_definitions: str | None
    documentation: str | None


def _summarize_contents(row: _SnapshotContentRow) -> dict:
    """Layer 2: structural summary — types/counts/names, no raw content."""
    summary: dict = {}

    if row.selected_events:
        try:
            events = json.loads(row.selected_events)
            by_type: dict[str, int] = {}
            for e in events:
                et = e.get("event_type", "unknown")
                by_type[et] = by_type.get(et, 0) + 1
            summary["events"] = {"total": len(events), "by_type": by_type}
        except (ValueError, TypeError):
            pass

    if row.code_context:
        try:
            code = json.loads(row.code_context)
            summary["code"] = {
                "files": len(code),
                "paths": [c.get("file", c.get("path", "?")) for c in code][:10],
            }
        except (ValueError, TypeError):
            pass

    if row.skill_definitions:
        try:
            skills = json.loads(row.skill_definitions)
            summary["skills"] = [s.get("skill_name", "?") for s in skills]
        except (ValueError, TypeError):
            pass

    if row.documentation:
        try:
            docs = json.loads(row.documentation)
            summary["docs"] = [d.get("source", d.get("title", "?")) for d in docs][:10]
        except (ValueError, TypeError):
            pass

    return summary


def _raw_contents(row: _SnapshotContentRow, token_budget: int) -> dict:
    """Layer 3: actual content, truncated to fit within token_budget."""
    char_budget = token_budget * TOKEN_CHAR_RATIO
    used = 0
    raw: dict = {}

    for key, blob in [("events", row.selected_events), ("code", row.code_context),
                       ("skills", row.skill_definitions), ("docs", row.documentation)]:
        if not blob or used >= char_budget:
            continue
        try:
            data = json.loads(blob)
        except (ValueError, TypeError):
            continue
        serialized = json.dumps(data, ensure_ascii=False)
        if len(serialized) <= char_budget - used:
            raw[key] = data
            used += len(serialized)
        else:
            items = []
            for item in data:
                item_str = json.dumps(item, ensure_ascii=False)
                if used + len(item_str) > char_budget:
                    break
                items.append(item)
                used += len(item_str)
            if items:
                raw[key] = items
                raw[f"{key}_truncated"] = True

    return raw


# ---------------------------------------------------------------------------
# Endpoints
# ---------------------------------------------------------------------------

@router.get("/introspection/memory")
def get_introspection_memory(
    session_id: str = Query(..., description="Session ID to query"),
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
) -> dict:
    """Return memory stats for introspection tool."""
    _verify_session_owner(db, session_id, current_user["user_id"])
    return {
        "episodic": _get_episodic_stats(db, session_id),
        "semantic": _get_semantic_stats(db, session_id),
        "procedural": _get_procedural_stats(db, session_id),
    }


def _get_episodic_stats(db: Session, session_id: str) -> dict:
    """Conversation activity summary with derived signals."""
    try:
        row = db.execute(
            text("""
                SELECT
                    COUNT(*) as total,
                    SUM(CASE WHEN event_type = 'user_query' THEN 1 ELSE 0 END) as user_queries,
                    SUM(CASE WHEN event_type = 'tool_call' OR event_type = 'tool_result' THEN 1 ELSE 0 END) as tool_calls
                FROM agent_events
                WHERE session_id = :sid
            """),
            {"sid": session_id},
        ).fetchone()
        total = row[0] or 0
        turns = row[1] or 0
        tool_calls = row[2] or 0

        tool_ratio = round(tool_calls / max(total, 1), 2)
        tool_intensity = "high" if tool_ratio > 0.5 else ("medium" if tool_ratio > 0.2 else "low")
        depth = "deep" if turns >= 10 else ("moderate" if turns >= 4 else "shallow")

        return {
            "turns": turns,
            "total_events": total,
            "tool_intensity": tool_intensity,
            "session_depth": depth,
        }
    except Exception as exc:
        logger.warning("episodic stats query failed: %s", exc)
        return {"turns": 0, "total_events": 0, "tool_intensity": "low", "session_depth": "shallow"}


def _get_semantic_stats(db: Session, session_id: str) -> dict:
    """Compute context health conclusions from ctx_snapshots."""
    try:
        rows = db.execute(
            text("""
                SELECT token_budget, total_tokens, assembly_time_ms, created_at
                FROM ctx_snapshots
                WHERE session_id = :sid
                ORDER BY created_at DESC
            """),
            {"sid": session_id},
        ).fetchall()

        count = len(rows)
        token_history = [r[1] for r in rows if r[1] is not None]
        result: dict = {
            "ctx_snapshots": count,
            "peak_tokens": max(token_history, default=0),
        }

        if rows:
            latest = rows[0]
            if latest[1] is not None:
                result["current_tokens"] = latest[1]
            if latest[2] is not None:
                result["last_assembly_ms"] = latest[2]
            if latest[0] is not None:
                try:
                    budget = json.loads(latest[0])
                    result["health"] = _analyze_context_health(budget, token_history)
                except (ValueError, TypeError):
                    pass

        return result
    except Exception as exc:
        logger.warning("semantic stats query failed: %s", exc)
        return {"ctx_snapshots": 0, "peak_tokens": 0}


def _get_procedural_stats(db: Session, session_id: str) -> dict:
    """Get skill selection accuracy for this session."""
    try:
        row = db.execute(
            text("""
                SELECT COUNT(*),
                       SUM(CASE WHEN user_feedback_score > 0 THEN 1 ELSE 0 END)
                FROM skill_selection_events
                WHERE session_id = :sid
            """),
            {"sid": session_id},
        ).fetchone()
        total = row[0] or 0
        positive = row[1] or 0
        return {
            "skill_selections": total,
            "accuracy_rate": round(positive / total, 2) if total >= 10 else None,
        }
    except Exception as exc:
        logger.warning("procedural stats query failed: %s", exc)
        return {"skill_selections": 0, "accuracy_rate": None}


@router.get("/introspection/skills")
def get_introspection_skills(
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
) -> dict:
    """Return user-installed skills and available cloud skills."""
    user_id = current_user["user_id"]
    try:
        installed_rows = db.execute(
            text("""
                SELECT i.skill_name, i.skill_version, r.description, r.category
                FROM skill_installations i
                LEFT JOIN skills_registry r
                    ON r.skill_name = i.skill_name
                    AND r.version = i.skill_version
                    AND r.is_active = 1
                WHERE i.user_id = :uid AND i.status = 'installed'
                LIMIT 50
            """),
            {"uid": user_id},
        ).fetchall()
        installed = [
            {"name": r[0], "version": r[1], "description": r[2] or "", "category": r[3] or ""}
            for r in installed_rows
        ]
        installed_names = {r[0] for r in installed_rows}

        cloud_rows = db.execute(
            text("""
                SELECT skill_name, version, description, category
                FROM skills_registry
                WHERE is_active = 1
                ORDER BY skill_name, version DESC
                LIMIT 200
            """),
        ).fetchall()
        seen: set[str] = set()
        cloud = []
        for r in cloud_rows:
            if r[0] in seen or r[0] in installed_names:
                continue
            seen.add(r[0])
            cloud.append({"name": r[0], "version": r[1], "description": r[2] or "", "category": r[3] or ""})

        return {"installed": installed, "cloud": cloud}
    except SQLAlchemyError as exc:
        logger.warning("introspection skills query failed: %s", exc)
        return {"installed": [], "cloud": []}


@router.get("/introspection/context/trend")
def get_context_trend(
    session_id: str = Query(...),
    turns: int = Query(10, ge=2, le=50),
    compaction_limit: int = Query(12000),
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
) -> dict:
    """Token usage trend + compaction forecast for the last N turns."""
    _verify_session_owner(db, session_id, current_user["user_id"])
    rows = db.execute(
        text("""
            SELECT total_tokens FROM ctx_snapshots
            WHERE session_id = :sid
            ORDER BY created_at DESC
            LIMIT :n
        """),
        {"sid": session_id, "n": turns},
    ).fetchall()

    if not rows:
        return {"turns_sampled": 0, "trend": "no_data"}

    token_history = [r[0] for r in rows if r[0] is not None]
    trend = _compute_trend(token_history)

    return {
        "turns_sampled": len(rows),
        "trend": trend,
        "current_tokens": token_history[0] if token_history else None,
        "forecast": _compaction_forecast(token_history, compaction_limit),
        "compaction_history": _compaction_effectiveness(token_history),
    }


@router.get("/introspection/context/snapshot")
def get_context_snapshot(
    session_id: str = Query(...),
    turn_index: int = Query(None, ge=1),
    detail: bool = Query(False),
    raw: bool = Query(False),
    raw_token_budget: int = Query(2000, ge=100, le=8000),
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
) -> dict:
    """Layered context snapshot for a specific turn.

    Layer 1 (default): health, relevance, pollution, zone balance.
    Layer 2 (detail=true): + structural summary.
    Layer 3 (raw=true): + actual content truncated to raw_token_budget.
    """
    _verify_session_owner(db, session_id, current_user["user_id"])

    # Get total count + target row in 2 efficient queries (not fetchall)
    total_turns = db.execute(
        text("SELECT COUNT(*) FROM ctx_snapshots WHERE session_id = :sid"),
        {"sid": session_id},
    ).scalar() or 0

    if total_turns == 0:
        raise HTTPException(status_code=404, detail="No snapshots for this session")

    # Resolve offset: turn_index is 1-based oldest-first; DB is ordered ASC
    if turn_index:
        if turn_index > total_turns:
            raise HTTPException(status_code=404, detail=f"Turn {turn_index} not found (session has {total_turns} turns)")
        offset = turn_index - 1
        actual_turn = turn_index
    else:
        offset = total_turns - 1  # latest
        actual_turn = total_turns

    # Fetch only the target row
    content_cols = ", selected_events, code_context, skill_definitions, documentation" if (detail or raw) else ""
    row = db.execute(
        text(f"""
            SELECT context_capture_id, token_budget, total_tokens,
                   assembly_time_ms, relevance_scores, task_type{content_cols}
            FROM ctx_snapshots
            WHERE session_id = :sid
            ORDER BY created_at ASC
            LIMIT 1 OFFSET :off
        """),
        {"sid": session_id, "off": offset},
    ).fetchone()

    # Layer 1: conclusions
    result: dict = {
        "snapshot_id": row[0],
        "turn": actual_turn,
        "total_turns": total_turns,
        "task_type": row[5],
        "total_tokens": row[2],
        "assembly_ms": row[3],
    }

    if row[1]:
        try:
            budget = json.loads(row[1])
            # For health trend, fetch just total_tokens column (lightweight)
            token_rows = db.execute(
                text("SELECT total_tokens FROM ctx_snapshots WHERE session_id = :sid ORDER BY created_at DESC"),
                {"sid": session_id},
            ).fetchall()
            token_history = [r[0] for r in token_rows if r[0] is not None]
            result["health"] = _analyze_context_health(budget, token_history)
            result["zone_balance"] = _zone_balance(budget, row[5])
        except (ValueError, TypeError):
            pass

    if row[4]:
        try:
            scores = json.loads(row[4])
            result["relevance"] = _relevance_quality(scores)
            result["pollution"] = _pollution_ratio(scores)
        except (ValueError, TypeError):
            pass

    # Layer 2 & 3
    if detail or raw:
        content = _SnapshotContentRow(
            selected_events=row[6],
            code_context=row[7],
            skill_definitions=row[8],
            documentation=row[9],
        )
        result["contents"] = _summarize_contents(content)
        if raw:
            result["raw"] = _raw_contents(content, raw_token_budget)

    return result


@router.get("/introspection/context/retrieval_quality")
def get_retrieval_quality(
    session_id: str = Query(...),
    turns: int = Query(5, ge=1, le=20),
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
) -> dict:
    """Relevance score trend across recent turns — detects retrieval degradation."""
    _verify_session_owner(db, session_id, current_user["user_id"])
    rows = db.execute(
        text("""
            SELECT relevance_scores FROM ctx_snapshots
            WHERE session_id = :sid
            ORDER BY created_at DESC
            LIMIT :n
        """),
        {"sid": session_id, "n": turns},
    ).fetchall()

    if not rows:
        return {"turns_sampled": 0, "overall_quality": "no_data"}

    means = []
    for r in reversed(rows):
        if r[0]:
            try:
                q = _relevance_quality(json.loads(r[0]))
                if q["mean"] is not None:
                    means.append(q["mean"])
            except (ValueError, TypeError):
                pass

    if not means:
        return {"turns_sampled": len(rows), "overall_quality": "no_data"}

    overall_mean = round(sum(means) / len(means), 3)
    degrading = len(means) >= 2 and (means[0] - means[-1]) > DEGRADATION_DELTA
    overall_quality = "degrading" if degrading else (
        "good" if overall_mean >= QUALITY_GOOD else ("degraded" if overall_mean >= QUALITY_DEGRADED else "poor")
    )

    return {
        "turns_sampled": len(rows),
        "overall_quality": overall_quality,
        "mean_relevance": overall_mean,
        "recommendation": "consider context reset or re-retrieval" if overall_quality in ("degrading", "poor") else "retrieval healthy",
    }
