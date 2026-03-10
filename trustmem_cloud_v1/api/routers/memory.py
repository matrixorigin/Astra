"""Memory CRUD + retrieval endpoints (SaaS version, no experiments)."""

from __future__ import annotations

from datetime import datetime
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel, Field
from sqlalchemy.orm import Session

from trustmem_cloud_v1.api.database import get_db_factory, get_db_session
from trustmem_cloud_v1.api.dependencies import get_current_user_id

router = APIRouter(tags=["memory"])


# ── Schemas ───────────────────────────────────────────────────────────

class StoreRequest(BaseModel):
    content: str = Field(..., min_length=1)
    memory_type: str = Field(default="semantic")
    trust_tier: str | None = None
    session_id: str | None = None
    source: str = "api"


class BatchStoreRequest(BaseModel):
    memories: list[StoreRequest] = Field(..., min_length=1)


class RetrieveRequest(BaseModel):
    query: str = Field(..., min_length=1)
    top_k: int = Field(default=10, ge=1, le=100)
    memory_types: list[str] | None = None
    session_id: str | None = None
    include_cross_session: bool = True


class CorrectRequest(BaseModel):
    new_content: str = Field(..., min_length=1)
    reason: str = ""


class PurgeRequest(BaseModel):
    memory_ids: list[str] | None = None
    memory_types: list[str] | None = None
    before: datetime | None = None
    reason: str = ""


class SearchRequest(BaseModel):
    query: str = Field(..., min_length=1)
    top_k: int = Field(default=10, ge=1, le=100)


class ObserveRequest(BaseModel):
    messages: list[dict[str, Any]] = Field(..., min_length=1)
    source_event_ids: list[str] | None = None


# ── Helpers ───────────────────────────────────────────────────────────

def _to_response(mem: Any) -> dict[str, Any]:
    return {
        "memory_id": mem.memory_id,
        "content": mem.content,
        "memory_type": str(mem.memory_type) if mem.memory_type else "fact",
        "trust_tier": str(mem.trust_tier) if hasattr(mem, "trust_tier") and mem.trust_tier else None,
        "confidence": getattr(mem, "initial_confidence", None),
        "observed_at": mem.observed_at.isoformat() if hasattr(mem, "observed_at") and mem.observed_at else None,
    }


def _get_editor(db_factory, user_id: str):
    from core.memory.factory import create_editor
    return create_editor(db_factory, user_id=user_id)


def _verify_ownership(db_factory, memory_id: str, user_id: str):
    """Verify memory belongs to user. Raises 404 if not found or not owned."""
    from sqlalchemy import text
    db = db_factory()
    try:
        row = db.execute(
            text("SELECT user_id FROM mem_memories WHERE memory_id = :mid AND is_active = 1"),
            {"mid": memory_id},
        ).first()
        if row is None or row[0] != user_id:
            raise HTTPException(status_code=404, detail="Memory not found")
    finally:
        db.close()


def _get_service(db_factory, user_id: str):
    from core.memory.factory import create_memory_service
    return create_memory_service(db_factory, user_id=user_id)


# ── Endpoints ─────────────────────────────────────────────────────────

@router.get("/memories")
def list_memories(
    memory_type: str | None = None,
    limit: int = 100,
    offset: int = 0,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """List active memories for the current user."""
    from sqlalchemy import text
    db = db_factory()
    try:
        where = "user_id = :uid AND is_active = 1"
        params: dict = {"uid": user_id, "limit": limit, "offset": offset}
        if memory_type:
            where += " AND memory_type = :mt"
            params["mt"] = memory_type
        rows = db.execute(
            text(f"SELECT memory_id, content, memory_type, initial_confidence, observed_at FROM mem_memories WHERE {where} ORDER BY observed_at DESC LIMIT :limit OFFSET :offset"),
            params,
        ).fetchall()
        return [
            {"memory_id": r[0], "content": r[1], "memory_type": r[2], "confidence": r[3],
             "observed_at": r[4].isoformat() if r[4] else None}
            for r in rows
        ]
    finally:
        db.close()


@router.post("/memories", status_code=status.HTTP_201_CREATED)
def store_memory(
    req: StoreRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    from core.memory.types import MemoryType, TrustTier
    editor = _get_editor(db_factory, user_id)
    try:
        mem = editor.inject(
            user_id, req.content,
            memory_type=MemoryType(req.memory_type),
            trust_tier=TrustTier(req.trust_tier) if req.trust_tier else None,
            source=req.source, session_id=req.session_id,
        )
    except ValueError as e:
        raise HTTPException(status_code=422, detail=str(e))
    return _to_response(mem)


@router.post("/memories/batch", status_code=status.HTTP_201_CREATED)
def batch_store(
    req: BatchStoreRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    from core.memory.types import MemoryType
    editor = _get_editor(db_factory, user_id)
    specs = [
        {"content": m.content, "memory_type": MemoryType(m.memory_type), "source": m.source}
        for m in req.memories
    ]
    memories = editor.batch_inject(user_id, specs, source="api_batch")
    return [_to_response(m) for m in memories]


@router.post("/memories/retrieve")
def retrieve_memories(
    req: RetrieveRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    from core.memory.types import MemoryType
    svc = _get_service(db_factory, user_id=user_id)
    memory_types = [MemoryType(t) for t in req.memory_types] if req.memory_types else None
    memories, _meta = svc.retrieve(
        user_id, req.query, top_k=req.top_k, memory_types=memory_types,
        session_id=req.session_id or "", include_cross_session=req.include_cross_session,
    )
    return [_to_response(m) for m in memories]


@router.post("/memories/search")
def search_memories(
    req: SearchRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    svc = _get_service(db_factory, user_id=user_id)
    memories, _meta = svc.retrieve(user_id, req.query, top_k=req.top_k)
    return [_to_response(m) for m in memories]


@router.put("/memories/{memory_id}/correct")
def correct_memory(
    memory_id: str,
    req: CorrectRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    _verify_ownership(db_factory, memory_id, user_id)
    editor = _get_editor(db_factory, user_id)
    try:
        mem = editor.correct(user_id, memory_id, req.new_content, reason=req.reason)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))
    return _to_response(mem)


@router.delete("/memories/{memory_id}")
def delete_memory(
    memory_id: str,
    reason: str = "",
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    _verify_ownership(db_factory, memory_id, user_id)
    editor = _get_editor(db_factory, user_id)
    result = editor.purge(user_id, memory_ids=[memory_id], reason=reason)
    return {"purged": result.deactivated}


@router.post("/memories/purge")
def purge_memories(
    req: PurgeRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    from core.memory.types import MemoryType
    editor = _get_editor(db_factory, user_id)
    memory_types = [MemoryType(t) for t in req.memory_types] if req.memory_types else None
    result = editor.purge(
        user_id, memory_ids=req.memory_ids, memory_types=memory_types,
        before=req.before, reason=req.reason,
    )
    return {"purged": result.deactivated}


@router.get("/profiles/{target_user_id}")
def get_profile(
    target_user_id: str,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    # "me" resolves to the authenticated user
    resolved = user_id if target_user_id == "me" else target_user_id
    svc = _get_service(db_factory, user_id=resolved)
    profile = svc.get_profile(resolved)
    return {"user_id": resolved, "profile": profile}


@router.post("/observe")
def observe_turn(
    req: ObserveRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    svc = _get_service(db_factory, user_id=user_id)
    memories = svc.observe_turn(user_id, req.messages, source_event_ids=req.source_event_ids)
    return [_to_response(m) for m in memories]
