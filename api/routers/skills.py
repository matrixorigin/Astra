"""Skill API Router — skill registration, listing, versioning, publish/unpublish.

Route ordering note: static path segments (/status, /publish) MUST be defined
before parameterized segments (/{skill_id}, /{skill_name}/...) to avoid
FastAPI matching "status" as a skill_id.
"""

from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query, status
from pydantic import BaseModel

from api.database import SessionLocal
from api.dependencies import get_current_user
from core.exceptions import SkillNotFoundError
from core.skills.catalog import NameConflictError, SkillCatalog

router = APIRouter()

# Module-level singleton — the in-memory _skills dict and metadata cache
# only provide value when the same instance is reused across requests.
_catalog_instance: SkillCatalog | None = None


def _catalog() -> SkillCatalog:
    global _catalog_instance
    if _catalog_instance is None:
        _catalog_instance = SkillCatalog(SessionLocal)
    return _catalog_instance


def reset_catalog() -> None:
    """Reset the singleton for testing.

    Prefer manipulating ``_catalog_instance`` directly from test fixtures
    instead of calling this.  Kept as a convenience for integration tests
    that import it.
    """
    global _catalog_instance
    _catalog_instance = None


# ── Request / Response models ─────────────────────────────────────────────────


class RegisterSkillRequest(BaseModel):
    skill_id: str
    skill_name: str
    skill_version: str
    skill_code: str
    description: str | None = None
    metadata: dict[str, Any] | None = None


class PublishSkillRequest(BaseModel):
    name: str
    version: str
    description: str
    triggers: list[str] | None = None
    dependencies: list[str] | None = None
    manifest: dict[str, Any] | None = None
    category: str = "user"
    priority: int = 5


class SkillResponse(BaseModel):
    skill_id: str
    skill_name: str
    version: str
    description: str | None = None
    metadata: dict[str, Any] | None = None
    created_at: str | None = None


class SkillListResponse(BaseModel):
    skills: list[dict[str, Any]]
    total: int
    limit: int
    offset: int


class SkillVersionResponse(BaseModel):
    version: str
    status: str | None = None
    is_active: int | None = None
    created_at: str | None = None


class SkillInfoResponse(BaseModel):
    skill_name: str
    version: str
    description: str | None = None
    source: str | None = None
    status: str | None = None
    created_by: str | None = None
    category: str | None = None
    install_count: int = 0
    created_at: str | None = None


class SkillStatusResponse(BaseModel):
    builtin: list[dict[str, Any]]
    marketplace: list[dict[str, Any]]
    user: list[dict[str, Any]]
    platform_total: int = 0
    user_total: int = 0


# ── CRUD endpoints ────────────────────────────────────────────────────────────


@router.post("", response_model=SkillResponse, status_code=status.HTTP_201_CREATED)
async def register_skill(
    request: RegisterSkillRequest,
    current_user: dict = Depends(get_current_user),
):
    """Register a skill (admin/platform use)."""
    try:
        result = _catalog().register_from_api(
            skill_id=request.skill_id,
            skill_name=request.skill_name,
            version=request.skill_version,
            skill_code=request.skill_code,
            description=request.description or "",
            metadata=request.metadata,
            created_by=current_user.get("user_id"),
        )
        return SkillResponse(**result)
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_400_BAD_REQUEST, detail=str(e))
    except NameConflictError as e:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail=str(e))


@router.get("", response_model=SkillListResponse)
async def list_skills(
    limit: int = Query(50, ge=1, le=100),
    offset: int = Query(0, ge=0),
    current_user: dict = Depends(get_current_user),
):
    """List active skills."""
    return _catalog().list_active(limit=limit, offset=offset)


# Static routes BEFORE parameterized routes — see module docstring.

@router.get("/status", response_model=SkillStatusResponse)
async def get_skill_status(
    per_group: int = Query(50, ge=1, le=200),
    current_user: dict = Depends(get_current_user),
):
    """Get all skills visible to the current user, grouped by source."""
    return _catalog().get_visible_skills(current_user["user_id"], per_group=per_group)


@router.post("/publish", status_code=status.HTTP_201_CREATED)
async def publish_skill(
    req: PublishSkillRequest,
    current_user: dict = Depends(get_current_user),
):
    """Publish a user-created skill to the platform."""
    try:
        return _catalog().publish_user_skill(
            user_id=current_user["user_id"],
            name=req.name,
            version=req.version,
            description=req.description,
            triggers=req.triggers,
            dependencies=req.dependencies,
            manifest=req.manifest,
            category=req.category,
            priority=req.priority,
        )
    except NameConflictError as e:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail=str(e))


@router.post("/scaffold")
async def scaffold_skill(spec_data: dict[str, Any]):
    """Generate skill package from YAML spec. Returns file contents as JSON."""
    from core.skills.scaffold import SkillSpec, generate_files

    try:
        spec = SkillSpec.from_dict(spec_data)
        return generate_files(spec)
    except ValueError as e:
        raise HTTPException(status_code=422, detail=str(e))


# Parameterized routes AFTER static routes.

@router.get("/{skill_name}/info", response_model=SkillInfoResponse)
async def get_skill_info(
    skill_name: str,
    current_user: dict = Depends(get_current_user),
):
    """Get detailed skill info including install count."""
    info = _catalog().get_skill_info(skill_name, current_user["user_id"])
    if not info:
        raise HTTPException(status_code=404, detail=f"Skill '{skill_name}' not found")
    return info


@router.get("/{skill_id}", response_model=SkillResponse)
async def get_skill(
    skill_id: str,
    version: str | None = Query(None),
    current_user: dict = Depends(get_current_user),
):
    """Get skill by ID or name.

    The path param may be a full skill_id (primary key, e.g. "name@1.0.0"),
    a bare skill name, or an opaque ID assigned at registration time.
    We try: exact skill_id match first, then active-version-by-name.
    """
    catalog = _catalog()

    # 1. Try exact skill_id match (primary key lookup)
    meta = catalog.get_metadata_by_id(skill_id)

    # 2. Fall back to name-based lookup (active version)
    if meta is None:
        name = skill_id.split("@")[0] if "@" in skill_id else skill_id
        meta = catalog.get_metadata(name)

    if not meta:
        raise HTTPException(status_code=404, detail=f"Skill '{skill_id}' not found")
    return SkillResponse(
        skill_id=meta.get("skill_id", skill_id),
        skill_name=meta["skill_name"],
        version=meta["version"],
        description=meta.get("description"),
        metadata=meta.get("skill_definition"),
        created_at=meta.get("created_at"),
    )


@router.get("/{skill_id}/versions", response_model=list[SkillVersionResponse])
async def list_skill_versions(
    skill_id: str,
    current_user: dict = Depends(get_current_user),
):
    """List all versions of a skill."""
    name = skill_id.split("@")[0] if "@" in skill_id else skill_id
    return _catalog().list_versions(name)


# Unpublish is a state transition (may deprecate instead of delete),
# so POST is more appropriate than DELETE.
@router.post("/{skill_name}/unpublish")
async def unpublish_skill(
    skill_name: str,
    current_user: dict = Depends(get_current_user),
):
    """Unpublish a user skill."""
    try:
        result = _catalog().unpublish_user_skill(current_user["user_id"], skill_name)
        return {"skill_name": skill_name, "result": result}
    except SkillNotFoundError:
        raise HTTPException(status_code=404, detail=f"Skill '{skill_name}' not found")
