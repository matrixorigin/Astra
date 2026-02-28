"""Introspection API — cloud-side data for get_agent_info tool."""

from fastapi import APIRouter, Depends, HTTPException, Query
from sqlalchemy import text
from sqlalchemy.exc import SQLAlchemyError
from sqlalchemy.orm import Session

from api.database import SessionLocal
from api.dependencies import get_current_user
from core.logging_config import get_logger

logger = get_logger(__name__)
router = APIRouter()


@router.get("/introspection/memory")
def get_introspection_memory(
    session_id: str = Query(..., description="Session ID to query"),
    current_user: dict = Depends(get_current_user),
) -> dict:
    """Return memory stats for introspection tool."""
    user_id = current_user["user_id"]
    db = SessionLocal()
    try:
        session_row = db.execute(
            text("SELECT user_id FROM agent_sessions WHERE session_id = :sid"),
            {"sid": session_id},
        ).fetchone()
        if not session_row or session_row[0] != user_id:
            raise HTTPException(status_code=404, detail="Session not found")

        return {
            "episodic": _get_episodic_stats(db, session_id),
            "semantic": _get_semantic_stats(db, session_id),
            "procedural": _get_procedural_stats(db, session_id),
        }
    finally:
        db.close()


def _get_episodic_stats(db: Session, session_id: str) -> dict:
    """Count conversation events by type for this session."""
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
        return {
            "total_events": row[0] or 0,
            "user_queries": row[1] or 0,
            "tool_calls": row[2] or 0,
        }
    except Exception as exc:
        logger.warning("episodic stats query failed: %s", exc)
        return {"total_events": 0, "user_queries": 0, "tool_calls": 0}


def _get_semantic_stats(db: Session, session_id: str) -> dict:
    """Count context snapshots and peak token total.

    Uses total_tokens column from ctx_snapshots table
    (see api/models.py ContextSnapshot).
    """
    try:
        row = db.execute(
            text("""
                SELECT COUNT(*), MAX(total_tokens)
                FROM ctx_snapshots
                WHERE session_id = :sid
            """),
            {"sid": session_id},
        ).fetchone()
        return {
            "ctx_snapshots": row[0] or 0,
            "peak_snapshot_tokens": row[1] or 0,
        }
    except Exception as exc:
        logger.warning("semantic stats query failed: %s", exc)
        return {"ctx_snapshots": 0, "peak_snapshot_tokens": 0}


def _get_procedural_stats(db: Session, session_id: str) -> dict:
    """Get skill selection accuracy for this session.

    Scoped to session_id (consistent with episodic/semantic stats).
    Uses user_feedback_score (Integer, positive = good) from
    skill_selection_events (see api/models.py SkillSelectionEvent).
    """
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
) -> dict:
    """Return user-installed skills and available cloud skills for introspection.

    Two lists are returned so the LLM can distinguish personal skills
    from the global catalog when answering capability questions.
    Installed skills are excluded from the cloud list to avoid redundancy.
    """
    user_id = current_user["user_id"]
    db = SessionLocal()
    try:
        # User's installed skills.  JOIN on both skill_name AND version to
        # avoid cartesian product when multiple versions exist in the registry.
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
            {
                "name": r[0], "version": r[1],
                "description": r[2] or "", "category": r[3] or "",
            }
            for r in installed_rows
        ]
        installed_names = {r[0] for r in installed_rows}

        # Globally active cloud skills, deduplicated by skill_name (keep latest
        # version via ORDER BY version DESC), excluding already-installed skills.
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
            cloud.append({
                "name": r[0], "version": r[1],
                "description": r[2] or "", "category": r[3] or "",
            })

        return {"installed": installed, "cloud": cloud}
    except SQLAlchemyError as exc:
        logger.warning("introspection skills query failed: %s", exc)
        return {"installed": [], "cloud": []}
    finally:
        db.close()
