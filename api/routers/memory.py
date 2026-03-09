"""Memory REST API — standalone endpoints for memory service mode.

Provides CRUD + retrieval + observe for memories, independent of the chat router.
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel, Field

from api.dependencies import get_current_user_id, get_db_factory
from core.db_consumer import DbFactory
from core.logging_config import get_logger

logger = get_logger(__name__)
router = APIRouter()


# ── Request / Response schemas ────────────────────────────────────────

class StoreRequest(BaseModel):
    content: str = Field(..., min_length=1)
    memory_type: str = Field(default="fact")
    trust_tier: str | None = None
    session_id: str | None = None
    source: str = "api"


class BatchStoreRequest(BaseModel):
    memories: list[StoreRequest] = Field(..., min_items=1)


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
    memory_types: list[str] | None = None


class ObserveRequest(BaseModel):
    messages: list[dict[str, Any]] = Field(..., min_items=1)
    source_event_ids: list[str] | None = None


class ExperimentCreateRequest(BaseModel):
    name: str = Field(..., min_length=1)
    description: str = ""
    strategy_key: str | None = None


class MemoryResponse(BaseModel):
    memory_id: str
    content: str
    memory_type: str
    trust_tier: str | None = None
    confidence: float | None = None
    observed_at: str | None = None


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


def _get_service(db_factory: DbFactory, user_id: str):
    from core.memory.factory import create_memory_service
    return create_memory_service(db_factory, user_id=user_id)


def _get_editor(db_factory: DbFactory, user_id: str):
    from core.memory.factory import create_editor
    return create_editor(db_factory, user_id=user_id)


# ── Endpoints ─────────────────────────────────────────────────────────

@router.post("/memories", status_code=status.HTTP_201_CREATED)
def store_memory(
    req: StoreRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Store a memory."""
    from core.memory.types import MemoryType, TrustTier

    editor = _get_editor(db_factory, user_id)
    mem = editor.inject(
        user_id,
        req.content,
        memory_type=MemoryType(req.memory_type),
        trust_tier=TrustTier(req.trust_tier) if req.trust_tier else None,
        source=req.source,
        session_id=req.session_id,
    )
    return _to_response(mem)


@router.post("/memories/batch", status_code=status.HTTP_201_CREATED)
def batch_store(
    req: BatchStoreRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Batch store memories."""
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
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Retrieve relevant memories for a query."""
    from core.memory.types import MemoryType

    svc = _get_service(db_factory, user_id=user_id)
    memory_types = [MemoryType(t) for t in req.memory_types] if req.memory_types else None
    memories, _meta = svc.retrieve(
        user_id,
        req.query,
        top_k=req.top_k,
        memory_types=memory_types,
        session_id=req.session_id or "",
        include_cross_session=req.include_cross_session,
    )
    return [_to_response(m) for m in memories]


@router.put("/memories/{memory_id}/correct")
def correct_memory(
    memory_id: str,
    req: CorrectRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Correct an existing memory."""
    editor = _get_editor(db_factory, user_id)
    try:
        mem = editor.correct(user_id, memory_id, req.new_content, reason=req.reason)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))
    return _to_response(mem)


@router.delete("/memories/{memory_id}")
def purge_memory(
    memory_id: str,
    reason: str = "",
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Delete a specific memory."""
    editor = _get_editor(db_factory, user_id)
    result = editor.purge(user_id, memory_ids=[memory_id], reason=reason)
    return {"purged": result.count, "snapshot": result.snapshot_name}


@router.post("/memories/purge")
def purge_memories(
    req: PurgeRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Bulk purge memories by criteria."""
    from core.memory.types import MemoryType

    editor = _get_editor(db_factory, user_id)
    memory_types = [MemoryType(t) for t in req.memory_types] if req.memory_types else None
    result = editor.purge(
        user_id,
        memory_ids=req.memory_ids,
        memory_types=memory_types,
        before=req.before,
        reason=req.reason,
    )
    return {"purged": result.count, "snapshot": result.snapshot_name}


@router.post("/memories/search")
def search_memories(
    req: SearchRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Semantic search over memories."""
    svc = _get_service(db_factory, user_id=user_id)
    memories, _meta = svc.retrieve(user_id, req.query, top_k=req.top_k)
    return [_to_response(m) for m in memories]


@router.get("/profiles/{target_user_id}")
def get_profile(
    target_user_id: str,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Get user profile (memory-derived)."""
    svc = _get_service(db_factory, user_id=target_user_id)
    profile = svc.get_profile(target_user_id)
    return {"user_id": target_user_id, "profile": profile}


@router.post("/observe")
def observe_turn(
    req: ObserveRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Extract memories from a conversation turn."""
    svc = _get_service(db_factory, user_id=user_id)
    memories = svc.observe_turn(
        user_id,
        req.messages,
        source_event_ids=req.source_event_ids,
    )
    return [_to_response(m) for m in memories]


@router.post("/experiments", status_code=status.HTTP_201_CREATED)
def create_experiment(
    req: ExperimentCreateRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Create a memory experiment (sandbox branch)."""
    from core.memory.experiment import MemoryExperimentManager

    mgr = MemoryExperimentManager(db_factory)
    info = mgr.create(user_id, req.name, description=req.description, strategy_key=req.strategy_key)
    return {"experiment_id": info.experiment_id, "name": info.name, "status": info.status}


@router.get("/experiments/{experiment_id}")
def get_experiment(
    experiment_id: str,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Get experiment status."""
    from core.memory.experiment import MemoryExperimentManager

    mgr = MemoryExperimentManager(db_factory)
    info = mgr.get(experiment_id)
    if info is None:
        raise HTTPException(status_code=404, detail="Experiment not found")
    return {"experiment_id": info.experiment_id, "name": info.name, "status": info.status}


@router.post("/experiments/{experiment_id}/commit")
def commit_experiment(
    experiment_id: str,
    user_id: str = Depends(get_current_user_id),
    db_factory: DbFactory = Depends(get_db_factory),
):
    """Commit experiment changes to production."""
    from core.memory.experiment import MemoryExperimentManager

    mgr = MemoryExperimentManager(db_factory)
    try:
        mgr.commit(experiment_id)
    except ValueError as e:
        raise HTTPException(status_code=400, detail=str(e))
    return {"status": "committed"}
