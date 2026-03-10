"""Snapshot endpoints — MatrixOne native snapshots, read-only, no rollback, 100 per user."""

from __future__ import annotations

import re

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel, Field
from sqlalchemy import text
from sqlalchemy.orm import Session

from trustmem_cloud_v1.api.database import get_db_session
from trustmem_cloud_v1.api.dependencies import get_current_user_id
from trustmem_cloud_v1.api.models import SnapshotRegistry
from trustmem_cloud_v1.config import get_settings
from core.git_for_data import GitForData

router = APIRouter(tags=["snapshots"])


def _sanitize(name: str) -> str:
    safe = re.sub(r"[^a-zA-Z0-9_]", "_", name)
    if not safe:
        raise HTTPException(status_code=400, detail="Invalid snapshot name")
    return safe


def _snap_name(user_id: str, name: str) -> str:
    return f"mem_snap_{_sanitize(user_id)[:16]}_{_sanitize(name)}"


def _git(db_factory) -> GitForData:
    return GitForData(db_factory)


class CreateSnapshotRequest(BaseModel):
    name: str = Field(..., min_length=1, max_length=100)
    description: str = ""


class SnapshotResponse(BaseModel):
    name: str
    snapshot_name: str
    description: str | None = None
    timestamp: str


@router.post("/snapshots", response_model=SnapshotResponse, status_code=status.HTTP_201_CREATED)
def create_snapshot(
    req: CreateSnapshotRequest,
    user_id: str = Depends(get_current_user_id),
    db: Session = Depends(get_db_session),
):
    settings = get_settings()

    # Check limit via registry table
    count = db.query(SnapshotRegistry).filter_by(user_id=user_id).count()
    if count >= settings.snapshot_limit:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Snapshot limit reached ({settings.snapshot_limit}). Delete old snapshots first.",
        )

    snap_name = _snap_name(user_id, req.name)

    # Check uniqueness
    if db.query(SnapshotRegistry).filter_by(snapshot_name=snap_name).first():
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail=f"Snapshot '{req.name}' already exists")

    # Create MatrixOne native snapshot
    info = _git(lambda: db).create_snapshot(snap_name)

    # Register
    reg = SnapshotRegistry(
        snapshot_name=snap_name, user_id=user_id,
        display_name=req.name, description=req.description or None,
    )
    db.add(reg)
    db.commit()

    return SnapshotResponse(
        name=req.name, snapshot_name=snap_name,
        description=req.description or None,
        timestamp=str(info.get("timestamp", "")),
    )


@router.get("/snapshots", response_model=list[SnapshotResponse])
def list_snapshots(
    user_id: str = Depends(get_current_user_id),
    db: Session = Depends(get_db_session),
):
    rows = db.query(SnapshotRegistry).filter_by(user_id=user_id).order_by(SnapshotRegistry.created_at.desc()).all()
    return [
        SnapshotResponse(
            name=r.display_name, snapshot_name=r.snapshot_name,
            description=r.description,
            timestamp=r.created_at.isoformat() if r.created_at else "",
        )
        for r in rows
    ]


@router.get("/snapshots/{name}")
def get_snapshot(
    name: str,
    user_id: str = Depends(get_current_user_id),
    db: Session = Depends(get_db_session),
):
    """Read snapshot — query memories at snapshot point via time-travel."""
    snap_name = _snap_name(user_id, name)
    reg = db.query(SnapshotRegistry).filter_by(snapshot_name=snap_name, user_id=user_id).first()
    if reg is None:
        raise HTTPException(status_code=404, detail="Snapshot not found")

    # Capture ORM fields before raw SQL invalidates the session state
    display_name = reg.display_name
    description = reg.description

    # Get timestamp from MatrixOne
    git = _git(lambda: db)
    all_snaps = git.list_snapshots()
    snap_info = next((s for s in all_snaps if s["snapshot_name"] == snap_name), None)
    if snap_info is None:
        raise HTTPException(status_code=404, detail="Snapshot not found in database")

    ts = snap_info["timestamp"]

    # Use MatrixOne's {SNAPSHOT = 'name'} syntax for time-travel
    rows = db.execute(
        text(
            "SELECT memory_id, content, memory_type, initial_confidence "
            f"FROM mem_memories {{SNAPSHOT = '{snap_name}'}}"
            " WHERE user_id = :uid AND is_active = 1"
        ),
        {"uid": user_id},
    ).fetchall()

    return {
        "name": display_name,
        "snapshot_name": snap_name,
        "description": description,
        "timestamp": str(ts),
        "memory_count": len(rows),
        "memories": [
            {"memory_id": r[0], "content": r[1], "memory_type": r[2], "confidence": r[3]}
            for r in rows
        ],
    }


@router.delete("/snapshots/{name}", status_code=status.HTTP_204_NO_CONTENT)
def delete_snapshot(
    name: str,
    user_id: str = Depends(get_current_user_id),
    db: Session = Depends(get_db_session),
):
    snap_name = _snap_name(user_id, name)
    reg = db.query(SnapshotRegistry).filter_by(snapshot_name=snap_name, user_id=user_id).first()
    if reg is None:
        raise HTTPException(status_code=404, detail="Snapshot not found")

    # Drop MatrixOne native snapshot (DDL-like, needs clean transaction state)
    db.commit()
    db.execute(text(f"DROP SNAPSHOT {snap_name}"))
    # Remove registry entry
    db.delete(reg)
    db.commit()
