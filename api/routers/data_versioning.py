"""Data Versioning API — time-travel checkpoints, lineage, sandbox checkpoint/restore."""

from fastapi import APIRouter, Depends, HTTPException, Query
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user

router = APIRouter(prefix="/data-versioning", tags=["data-versioning"])


# ── Request / Response models ──────────────────────────────────────

class CreateCheckpointRequest(BaseModel):
    name: str = Field(..., min_length=1, max_length=128)
    description: str = ""


class CheckpointResponse(BaseModel):
    checkpoint_name: str
    timestamp: str | None = None
    description: str = ""


class EventAtCheckpoint(BaseModel):
    event_id: str
    session_id: str | None = None
    event_type: str | None = None
    content: str | None = None
    created_at: str | None = None


class LineageNode(BaseModel):
    event_id: str
    event_type: str | None = None
    content: str | None = None
    parent_event_id: str | None = None
    causal_chain_id: str | None = None
    created_at: str | None = None


class SandboxCheckpointRequest(BaseModel):
    checkpoint_name: str = Field(..., min_length=1, max_length=128)


# ── Checkpoints (time-travel) ─────────────────────────────────────

@router.post("/checkpoints", response_model=CheckpointResponse, status_code=201)
def create_checkpoint(
    req: CreateCheckpointRequest,
    db: Session = Depends(get_db_session),
    _user: dict = Depends(get_current_user),
):
    """Create a named checkpoint (MatrixOne snapshot) for time-travel queries."""
    from core.replay.time_machine import TimeMachine
    tm = TimeMachine(lambda: db)
    result = tm.create_checkpoint(req.name, req.description)
    return CheckpointResponse(
        checkpoint_name=result["checkpoint_name"],
        timestamp=str(result.get("timestamp", "")),
        description=req.description,
    )


@router.get("/checkpoints", response_model=list[CheckpointResponse])
def list_checkpoints(
    db: Session = Depends(get_db_session),
    _user: dict = Depends(get_current_user),
):
    """List all available checkpoints."""
    from core.replay.time_machine import TimeMachine
    tm = TimeMachine(lambda: db)
    return [
        CheckpointResponse(
            checkpoint_name=c.get("snapshot_name", c.get("name", "")),
            timestamp=str(c.get("timestamp", "")),
        )
        for c in tm.list_checkpoints()
    ]


@router.get("/checkpoints/{name}/events", response_model=list[EventAtCheckpoint])
def get_events_at_checkpoint(
    name: str,
    session_id: str | None = None,
    limit: int = Query(100, ge=1, le=1000),
    db: Session = Depends(get_db_session),
    _user: dict = Depends(get_current_user),
):
    """Time-travel query: read events as they were at a checkpoint (read-only)."""
    from core.replay.time_machine import TimeMachine
    tm = TimeMachine(lambda: db)
    try:
        events = tm.get_events_at_checkpoint(name, session_id=session_id, limit=limit)
    except Exception as e:
        raise HTTPException(400, f"Checkpoint query failed: {e}")
    return [
        EventAtCheckpoint(
            event_id=ev.event_id,
            session_id=ev.session_id,
            event_type=ev.event_type,
            content=ev.content[:500] if ev.content else None,
            created_at=str(ev.created_at) if ev.created_at else None,
        )
        for ev in events
    ]


# ── Lineage ────────────────────────────────────────────────────────

@router.get("/lineage/{event_id}/chain", response_model=list[LineageNode])
def get_causal_chain(
    event_id: str,
    db: Session = Depends(get_db_session),
    _user: dict = Depends(get_current_user),
):
    """Get the full causal chain for an event (upstream + downstream)."""
    from core.events.event_reader import EventReader
    reader = EventReader(lambda: db)

    # First get the event to find its causal_chain_id
    event = reader.get_event(event_id)
    if not event:
        raise HTTPException(404, f"Event {event_id} not found")
    if not event.causal_chain_id:
        return [_event_to_lineage(event)]

    chain = reader.get_causal_chain(event.causal_chain_id)
    return [_event_to_lineage(ev) for ev in chain]


@router.get("/lineage/{event_id}/upstream", response_model=list[LineageNode])
def trace_upstream(
    event_id: str,
    db: Session = Depends(get_db_session),
    _user: dict = Depends(get_current_user),
):
    """Trace upstream: walk parent_event_id chain to find all ancestors."""
    from core.events.event_reader import EventReader
    reader = EventReader(lambda: db)
    chain: list[LineageNode] = []
    current_id: str | None = event_id
    seen: set[str] = set()

    while current_id and current_id not in seen and len(chain) < 100:
        seen.add(current_id)
        ev = reader.get_event(current_id)
        if not ev:
            break
        chain.append(_event_to_lineage(ev))
        current_id = ev.parent_event_id

    return chain


# ── Sandbox checkpoint / restore ───────────────────────────────────

@router.post("/sandbox/{name}/checkpoint", status_code=201)
def sandbox_checkpoint(
    name: str,
    req: SandboxCheckpointRequest,
    db: Session = Depends(get_db_session),
    _user: dict = Depends(get_current_user),
):
    """Create a checkpoint within a sandbox."""
    from core.sandbox.sandbox import Sandbox
    sb = Sandbox(lambda: db)
    try:
        sb.snapshot(name, req.checkpoint_name)
    except Exception as e:
        raise HTTPException(400, f"Sandbox checkpoint failed: {e}")
    return {"sandbox": name, "checkpoint": req.checkpoint_name}


@router.post("/sandbox/{name}/restore")
def sandbox_restore(
    name: str,
    req: SandboxCheckpointRequest,
    db: Session = Depends(get_db_session),
    _user: dict = Depends(get_current_user),
):
    """Restore a sandbox to a previously created checkpoint."""
    from core.sandbox.sandbox import Sandbox
    sb = Sandbox(lambda: db)
    try:
        sb.restore(name, req.checkpoint_name)
    except Exception as e:
        raise HTTPException(400, f"Sandbox restore failed: {e}")
    return {"sandbox": name, "restored_to": req.checkpoint_name}


# ── Helpers ────────────────────────────────────────────────────────

def _event_to_lineage(ev) -> LineageNode:
    return LineageNode(
        event_id=ev.event_id,
        event_type=ev.event_type,
        content=ev.content[:500] if ev.content else None,
        parent_event_id=ev.parent_event_id,
        causal_chain_id=ev.causal_chain_id,
        created_at=str(ev.created_at) if ev.created_at else None,
    )
