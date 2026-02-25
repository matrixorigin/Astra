"""Introspection API — cloud-side data for get_agent_info tool."""

from fastapi import APIRouter, Depends, HTTPException, Query
from sqlalchemy import text
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from core.logging_config import get_logger

logger = get_logger(__name__)
router = APIRouter()


@router.get("/introspection/memory")
def get_introspection_memory(
    session_id: str = Query(..., description="Session ID to query"),
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
) -> dict:
    """Return memory stats for introspection tool.

    Provides episodic, semantic, and procedural memory statistics
    that the edge introspection tool can merge with local data.
    """
    user_id = current_user["user_id"]

    # Verify session belongs to user
    session_row = db.execute(
        text("SELECT user_id FROM sessions WHERE session_id = :sid"),
        {"sid": session_id},
    ).fetchone()
    if not session_row or session_row[0] != user_id:
        raise HTTPException(status_code=404, detail="Session not found")

    return {
        "episodic": _get_episodic_stats(db, session_id),
        "semantic": _get_semantic_stats(db, session_id),
        "procedural": _get_procedural_stats(db, user_id),
    }


def _get_episodic_stats(db: Session, session_id: str) -> dict:
    """Count conversation events by type for this session."""
    try:
        row = db.execute(
            text("""
                SELECT
                    COUNT(*) as total,
                    SUM(CASE WHEN event_type = 'user_query' THEN 1 ELSE 0 END) as user_queries,
                    SUM(CASE WHEN event_type = 'tool_call' OR event_type = 'tool_result' THEN 1 ELSE 0 END) as tool_calls
                FROM conversation_events
                WHERE session_id = :sid
            """),
            {"sid": session_id},
        ).fetchone()
        return {
            "total_events": row[0] or 0,
            "user_queries": row[1] or 0,
            "tool_calls": row[2] or 0,
        }
    except Exception as exc:
        logger.warning("episodic stats query failed: %s", exc)
        return {"total_events": 0, "user_queries": 0, "tool_calls": 0}


def _get_semantic_stats(db: Session, session_id: str) -> dict:
    """Count context snapshots and latest token total.

    Uses total_tokens column from context_snapshots table
    (see api/models.py ContextSnapshot).
    """
    try:
        row = db.execute(
            text("""
                SELECT COUNT(*), MAX(total_tokens)
                FROM context_snapshots
                WHERE session_id = :sid
            """),
            {"sid": session_id},
        ).fetchone()
        return {
            "context_snapshots": row[0] or 0,
            # MAX(total_tokens) returns the peak token count across all snapshots.
            "peak_snapshot_tokens": row[1] or 0,
        }
    except Exception as exc:
        logger.warning("semantic stats query failed: %s", exc)
        return {"context_snapshots": 0, "peak_snapshot_tokens": 0}


def _get_procedural_stats(db: Session, user_id: str) -> dict:
    """Get skill selection accuracy for this user.

    skill_selection_events has no user_id column, so we join through sessions.
    Uses user_feedback_score (Integer, positive = good) from
    skill_selection_events (see api/models.py SkillSelectionEvent).
    """
    try:
        row = db.execute(
            text("""
                SELECT COUNT(*),
                       SUM(CASE WHEN sse.user_feedback_score > 0 THEN 1 ELSE 0 END)
                FROM skill_selection_events sse
                JOIN sessions s ON sse.session_id = s.session_id
                WHERE s.user_id = :uid
            """),
            {"uid": user_id},
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
