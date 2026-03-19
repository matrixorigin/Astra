"""Three-level quality evaluation: Step → Chain → Session.

Ref: evaluation-and-evolution.md §2 — "Evaluate at three levels"

Step-level:   quality_score on each conversation_event (auto_scorer.py)
Chain-level:  aggregate step scores within a causal_chain, with cascade penalty
Session-level: aggregate chain scores within a session
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session, sessionmaker
from uuid_utils import uuid7

from core.logging_config import get_logger

logger = get_logger(__name__)

# Cascade penalty: if any step in a chain has quality < threshold,
# the chain score is penalised because errors propagate downstream.
_CASCADE_PENALTY_THRESHOLD = 2.5
_CASCADE_PENALTY_FACTOR = 0.15  # subtract per failing step


def score_chain(db: Session, causal_chain_id: str, session_id: str) -> dict[str, Any] | None:
    """Compute chain-level quality score from step scores.

    Returns dict with score, step_count, failure_count, details — or None if no scored steps.
    """
    db.expire_all()
    rows = _fetchall_with_fresh_session_retry(
        db,
        """
        SELECT event_id, quality_score
        FROM agent_events
        WHERE session_id = :sid
          AND causal_chain_id = :cid
          AND quality_score IS NOT NULL
        ORDER BY created_at ASC
        """,
        {"sid": session_id, "cid": causal_chain_id},
    )

    if not rows:
        return None

    scores = [float(r.quality_score) for r in rows]
    failures = [s for s in scores if s < _CASCADE_PENALTY_THRESHOLD]

    # Base: weighted mean (later steps matter more — they reflect accumulated quality)
    n = len(scores)
    weights = [1.0 + i * 0.5 for i in range(n)]
    total_w = sum(weights)
    base = sum(s * w for s, w in zip(scores, weights)) / total_w

    # Cascade penalty
    penalty = len(failures) * _CASCADE_PENALTY_FACTOR
    chain_score = round(max(0.0, min(5.0, base - penalty)), 2)

    result = {
        "score": chain_score,
        "step_count": n,
        "failure_count": len(failures),
        "details": {
            "base_score": round(base, 2),
            "cascade_penalty": round(penalty, 2),
            "step_scores": scores,
        },
    }

    _upsert_assessment(db, "chain", causal_chain_id, session_id, result)
    return result


def score_session(db: Session, session_id: str) -> dict[str, Any] | None:
    """Compute session-level quality score from chain assessments.

    Returns dict with score, chain_count, details — or None if no chain assessments.
    """
    db.expire_all()
    rows = _fetchall_with_fresh_session_retry(
        db,
        """
        SELECT target_id, score, step_count, failure_count
        FROM eval_quality_assessments
        WHERE session_id = :sid AND level = 'chain'
        ORDER BY created_at ASC
        """,
        {"sid": session_id},
    )

    if not rows:
        return None

    chain_scores = [float(r.score) for r in rows]
    chain_weights = [int(r.step_count) for r in rows]
    # Weight by step_count: longer chains contribute more
    total_w = sum(chain_weights) or 1
    session_score = round(
        sum(s * w for s, w in zip(chain_scores, chain_weights)) / total_w,
        2,
    )
    session_score = max(0.0, min(5.0, session_score))

    result = {
        "score": session_score,
        "chain_count": len(chain_scores),
        "details": {
            "chain_scores": chain_scores,
            "chain_weights": chain_weights,
        },
    }

    _upsert_assessment(db, "session", session_id, session_id, result)
    return result


def _upsert_assessment(
    db: Session,
    level: str,
    target_id: str,
    session_id: str,
    result: dict,
) -> None:
    """Insert or update a quality assessment row.

    MatrixOne does not support ON DUPLICATE KEY UPDATE when the conflict is on
    a unique key but not the primary key.  eval_quality_assessments has PK =
    assessment_id (always new uuid) and UK = (level, target_id), so we must
    use SELECT + INSERT/UPDATE.
    """
    now = datetime.now(timezone.utc)
    sc = result.get("step_count", result.get("chain_count", 0))
    fc = result.get("failure_count", 0)
    details = _json_dumps(result.get("details"))

    row = db.execute(
        text(
            "SELECT assessment_id FROM eval_quality_assessments "
            "WHERE level = :lvl AND target_id = :tid"
        ),
        {"lvl": level, "tid": target_id},
    ).fetchone()

    if row:
        db.execute(
            text(
                "UPDATE eval_quality_assessments "
                "SET score = :score, step_count = :sc, failure_count = :fc, "
                "details = :details, updated_at = :now "
                "WHERE assessment_id = :aid"
            ),
            {
                "score": result["score"],
                "sc": sc,
                "fc": fc,
                "details": details,
                "now": now,
                "aid": row.assessment_id,
            },
        )
    else:
        db.execute(
            text(
                "INSERT INTO eval_quality_assessments "
                "(assessment_id, level, target_id, session_id, score, "
                "step_count, failure_count, details, created_at, updated_at) "
                "VALUES (:aid, :lvl, :tid, :sid, :score, :sc, :fc, "
                ":details, :now, :now)"
            ),
            {
                "aid": str(uuid7()),
                "lvl": level,
                "tid": target_id,
                "sid": session_id,
                "score": result["score"],
                "sc": sc,
                "fc": fc,
                "details": details,
                "now": now,
            },
        )
    db.commit()
    db.expire_all()


def _fetchall_with_fresh_session_retry(
    db: Session, query: str, params: dict[str, Any]
) -> list[Any]:
    rows = db.execute(text(query), params).fetchall()
    if rows or not isinstance(db, Session):
        return rows

    fresh_db = sessionmaker(bind=db.get_bind(), expire_on_commit=False)()
    try:
        fresh_rows = fresh_db.execute(text(query), params).fetchall()
        if fresh_rows:
            logger.debug("Recovered empty evaluation query via fresh session retry")
        return fresh_rows
    finally:
        fresh_db.close()


def _json_dumps(obj: Any) -> str | None:
    if obj is None:
        return None
    import json

    return json.dumps(obj)
