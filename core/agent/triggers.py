"""Trigger management — webhook + cron schedule → AgentRun creation.

Triggers are persisted in the `triggers` table. Each trigger defines:
- What agent to run, with what input/context
- How it's activated: webhook (external POST) or schedule (cron)
- Owner (user_id) for authorization
"""

import hmac
import json
from datetime import datetime, timezone
from typing import Callable
from uuid import uuid4

from croniter import croniter
from sqlalchemy import text
from sqlalchemy.orm import Session

from core.logging_config import get_logger

logger = get_logger(__name__)

_VALID_TRIGGER_TYPES = {"webhook", "schedule"}


def create_trigger(
    db: Session,
    *,
    user_id: str,
    agent_id: str,
    trigger_type: str,  # "webhook" | "schedule"
    name: str,
    user_input: str,
    context: dict | None = None,
    cron_expr: str | None = None,
    session_id: str | None = None,
) -> dict:
    """Create a trigger. Returns trigger dict with secret for webhooks."""
    if trigger_type not in _VALID_TRIGGER_TYPES:
        raise ValueError(f"trigger_type must be one of {_VALID_TRIGGER_TYPES}")
    if trigger_type == "schedule" and not cron_expr:
        raise ValueError("cron_expr required for schedule triggers")
    if trigger_type == "schedule":
        if not croniter.is_valid(cron_expr):
            raise ValueError(f"Invalid cron expression: {cron_expr}")

    trigger_id = str(uuid4())[:12]
    secret = str(uuid4()) if trigger_type == "webhook" else None
    now = datetime.now(timezone.utc)

    # Compute next_fire for schedule triggers
    next_fire = None
    if trigger_type == "schedule":
        next_fire = croniter(cron_expr, now).get_next(datetime)

    db.execute(
        text(
            "INSERT INTO triggers "
            "(trigger_id, user_id, agent_id, trigger_type, name, user_input, context, "
            " cron_expr, secret, session_id, next_fire_at, is_active, created_at) "
            "VALUES (:tid, :uid, :aid, :tt, :name, :input, :ctx, "
            " :cron, :secret, :sid, :nf, 1, :now)"
        ),
        {
            "tid": trigger_id, "uid": user_id, "aid": agent_id,
            "tt": trigger_type, "name": name, "input": user_input,
            "ctx": json.dumps(context) if context else None,
            "cron": cron_expr, "secret": secret, "sid": session_id,
            "nf": next_fire, "now": now,
        },
    )
    db.commit()

    result = {
        "trigger_id": trigger_id, "trigger_type": trigger_type,
        "name": name, "agent_id": agent_id, "is_active": True,
    }
    if secret:
        result["secret"] = secret
        result["webhook_url"] = f"/triggers/{trigger_id}/fire"
    if next_fire:
        result["next_fire_at"] = next_fire.isoformat()
    return result


def get_trigger(db: Session, trigger_id: str) -> dict | None:
    row = db.execute(
        text("SELECT * FROM triggers WHERE trigger_id = :tid"),
        {"tid": trigger_id},
    ).mappings().first()
    return dict(row) if row else None


def list_triggers(db: Session, user_id: str) -> list[dict]:
    rows = db.execute(
        text("SELECT trigger_id, trigger_type, name, agent_id, is_active, cron_expr, next_fire_at "
             "FROM triggers WHERE user_id = :uid ORDER BY created_at DESC"),
        {"uid": user_id},
    ).mappings().fetchall()
    return [dict(r) for r in rows]


def delete_trigger(db: Session, trigger_id: str) -> bool:
    result = db.execute(
        text("DELETE FROM triggers WHERE trigger_id = :tid"),
        {"tid": trigger_id},
    )
    db.commit()
    return result.rowcount > 0


def fire_trigger(
    db_factory: Callable[[], Session],
    trigger_id: str,
    payload: dict | None = None,
) -> dict:
    """Fire a trigger → create an AgentRun. Returns run info.

    Uses *db_factory* (not a raw session) so that each internal operation
    gets its own short-lived session.  This prevents a failure here from
    corrupting the caller's session state (e.g. the trigger-loop's claim).
    """
    from core.agent.run import RunTrigger
    from core.agent.run_engine import RunEngine, _run_tasks
    import asyncio

    # Read trigger metadata with a short-lived session.
    db = db_factory()
    try:
        trig = get_trigger(db, trigger_id)
    finally:
        db.close()

    if not trig:
        raise ValueError(f"Trigger {trigger_id} not found")
    if not trig["is_active"]:
        raise ValueError(f"Trigger {trigger_id} is disabled")

    ctx = json.loads(trig["context"]) if isinstance(trig["context"], str) and trig["context"] else (trig["context"] or {})
    if payload:
        ctx["trigger_payload"] = payload

    trigger_type = RunTrigger.WEBHOOK if trig["trigger_type"] == "webhook" else RunTrigger.SCHEDULE

    engine = RunEngine(db_factory)
    run = engine.create_run(
        session_id=trig.get("session_id") or _auto_session(db_factory, trig["user_id"]),
        user_id=trig["user_id"],
        user_input=trig["user_input"],
        agent_id=trig["agent_id"],
        trigger=trigger_type,
        context=ctx,
    )

    task = asyncio.create_task(engine.start_run(run))
    _run_tasks[run.run_id] = task

    return {"run_id": run.run_id, "trigger_id": trigger_id, "status": run.status.value}


def advance_schedule(db: Session, trigger_id: str) -> None:
    """Update next_fire_at after a scheduled trigger fires."""
    trig = get_trigger(db, trigger_id)
    if not trig or not trig.get("cron_expr"):
        return
    now = datetime.now(timezone.utc)
    next_fire = croniter(trig["cron_expr"], now).get_next(datetime)
    db.execute(
        text("UPDATE triggers SET next_fire_at = :nf WHERE trigger_id = :tid"),
        {"nf": next_fire, "tid": trigger_id},
    )
    db.commit()


def claim_and_advance(db: Session, trigger_id: str) -> bool:
    """Atomically claim a due trigger and advance next_fire_at.

    Uses optimistic locking: UPDATE ... WHERE next_fire_at <= now.
    Only one worker succeeds (rowcount=1). Prevents duplicate fires.
    """
    trig = get_trigger(db, trigger_id)
    if not trig or not trig.get("cron_expr"):
        return False
    now = datetime.now(timezone.utc)
    next_fire = croniter(trig["cron_expr"], now).get_next(datetime)
    result = db.execute(
        text(
            "UPDATE triggers SET next_fire_at = :nf "
            "WHERE trigger_id = :tid AND next_fire_at <= :now"
        ),
        {"nf": next_fire, "tid": trigger_id, "now": now},
    )
    db.commit()
    return result.rowcount > 0


def verify_secret(provided: str, expected: str) -> bool:
    """Constant-time secret comparison to prevent timing attacks."""
    return hmac.compare_digest(provided, expected)


def get_due_triggers(db: Session) -> list[str]:
    """Get all active schedule triggers whose next_fire_at <= now."""
    rows = db.execute(
        text(
            "SELECT trigger_id FROM triggers "
            "WHERE trigger_type = 'schedule' AND is_active = 1 "
            "AND next_fire_at <= :now"
        ),
        {"now": datetime.now(timezone.utc)},
    ).fetchall()
    return [row[0] for row in rows]


def _auto_session(db_factory: Callable[[], Session], user_id: str) -> str:
    """Create a session for trigger-fired runs using a short-lived DB session."""
    from core.events.session_manager import SessionManager
    db = db_factory()
    try:
        mgr = SessionManager(db)
        session = mgr.create_session(user_id=user_id, metadata={"source": "trigger"})
        return session.session_id
    finally:
        db.close()
