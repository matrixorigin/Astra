"""Skill Configuration API — settings, secrets, resource bindings, validation.

Design doc: docs/design/skills-and-tools.md §13
Mounted at /skills (shares prefix with skills.py router).

Scope model:
  - "user"   — per-user setting, scope_id = user_id (default)
  - "global" — system-wide default, requires admin role
  Tenant scope is reserved for future multi-tenancy support and not
  exposed in the API until the tenant infrastructure is in place.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Literal

from fastapi import APIRouter, Depends, HTTPException, Query, status
from pydantic import BaseModel

from api.database import SessionLocal, get_db_session
from api.dependencies import get_current_user
from core.auth.permission_checker import PermissionChecker
from core.skills.config_center import CredentialManager, SkillConfigCenter

if TYPE_CHECKING:
    from sqlalchemy.orm import Session

router = APIRouter()

# Test-injection point: tests can set _center to override the shared singleton.
# In production this is always None and _get_center() uses get_config_center().
_center: SkillConfigCenter | None = None


def initialize() -> None:
    """Initialize the shared SkillConfigCenter singleton.

    Called once at application startup (from api/main.py lifespan).
    This is the only place that imports api.models and api.database into
    the config center initialization path — keeping core/ free of api/ imports.
    """
    import os
    from api.models.skill import SkillRegistry
    from core.skills.config_center import init_config_center

    key = os.environ.get("TOKEN_ENCRYPTION_KEY", "dev-key")
    cred_mgr = CredentialManager(key)

    def _manifest_loader(skill_name: str) -> dict | None:
        db = SessionLocal()
        try:
            row = db.query(
                SkillRegistry.manifest, SkillRegistry.skill_definition,
            ).filter(
                SkillRegistry.skill_name == skill_name,
                SkillRegistry.is_active == 1,
            ).order_by(SkillRegistry.created_at.desc()).first()
            if not row:
                return None
            if row[0]:
                return row[0]
            defn = row[1] or {}
            return defn.get("manifest") or (defn.get("settings") and defn) or None
        finally:
            db.close()

    init_config_center(SessionLocal, cred_mgr, _manifest_loader)


def _get_center() -> SkillConfigCenter:
    # Allow tests to inject a custom center (e.g. with a test manifest loader).
    # In production _center is None and we use the shared core-layer singleton
    # initialized by initialize() at startup.
    if _center is not None:
        return _center
    from core.skills.config_center import get_config_center
    return get_config_center()


from functools import lru_cache as _lru_cache

# Only "user" and "global" scopes are supported.
# Tenant scope is reserved — see module docstring.
ApiScope = Literal["user", "global"]


@_lru_cache(maxsize=256)
def _resolve_config_namespace(skill_name: str) -> str:
    """Resolve the config namespace for a skill (cached — namespaces are static).

    Skills that share credentials (e.g. all github skills) store config under
    a common namespace (e.g. 'github'). Resolution logic:
      1. If skill has its own manifest → it IS the namespace (return skill_name).
      2. Otherwise find another skill in the same category that has a manifest
         and return that manifest's 'name' field as the namespace.
      3. Fall back to skill_name if nothing found.

    Single query: fetch skill_name, category, manifest for all active skills
    in the same category as the requested skill, then resolve in Python.
    """
    from api.models.skill import SkillRegistry as SkillModel
    from core.logging_config import get_logger as _get_logger
    _log = _get_logger(__name__)

    db = SessionLocal()
    try:
        # Single query: get this skill's category + manifest, and all siblings' manifests.
        # We use a subquery to first get the category, then fetch all skills in that category.
        own = db.query(SkillModel.category, SkillModel.manifest).filter(
            SkillModel.skill_name == skill_name,
            SkillModel.is_active == 1,
        ).first()
        if not own:
            return skill_name

        # If this skill has its own manifest, it is the namespace.
        if own[1]:
            return skill_name

        category = own[0]
        if not category:
            return skill_name

        # Find any sibling in the same category that has a manifest.
        sibling = db.query(SkillModel.manifest).filter(
            SkillModel.category == category,
            SkillModel.is_active == 1,
            SkillModel.manifest.isnot(None),
        ).first()
        if sibling and sibling[0]:
            ns = sibling[0].get("name")
            if ns:
                return ns
    except Exception as e:
        _log.warning("_resolve_config_namespace(%r) failed: %s", skill_name, e)
    finally:
        db.close()
    return skill_name





def _resolve_scope(
    scope: ApiScope,
    user_id: str,
    db: Session,
) -> tuple[str, str | None]:
    """Return (scope_type, scope_id) and enforce admin for global scope."""
    if scope == "global":
        if not PermissionChecker(lambda: db).is_admin(user_id):
            raise HTTPException(
                status_code=status.HTTP_403_FORBIDDEN,
                detail="Admin role required for global scope",
            )
        return "global", None
    return "user", user_id


# ── Request / Response models ─────────────────────────────────────────────


class SetSettingRequest(BaseModel):
    value: str | int | float | bool


class BindResourceRequest(BaseModel):
    bindings: dict[str, str | int | float | bool]


class ConfigResponse(BaseModel):
    settings: dict[str, Any]
    secrets: dict[str, str]
    resources_configured: int


class ValidationResponse(BaseModel):
    valid: bool
    errors: list[dict[str, Any]]


# ── Validation (defined BEFORE /{setting_name} to avoid route shadowing) ──


@router.get("/{skill_name}/config/validate", response_model=ValidationResponse)
async def validate_config(
    skill_name: str,
    resource: str | None = Query(None, description="Resource key to validate"),
    current_user: dict = Depends(get_current_user),
):
    """Validate all required config is present."""
    center = _get_center()
    ns = _resolve_config_namespace(skill_name)
    errors = center.validate(ns, current_user["user_id"], resource_key=resource)
    return ValidationResponse(
        valid=len(errors) == 0,
        errors=[{"section": e.section, "name": e.name,
                 "resource_key": e.resource_key, "error": e.error} for e in errors],
    )


# ── Settings & Secrets ────────────────────────────────────────────────────


@router.get("/{skill_name}/config", response_model=ConfigResponse)
async def get_effective_config(
    skill_name: str,
    current_user: dict = Depends(get_current_user),
):
    """Get effective resolved config for current user (secrets masked)."""
    center = _get_center()
    ns = _resolve_config_namespace(skill_name)
    config = center.resolve_all(ns, current_user["user_id"])
    return ConfigResponse(
        settings=config.settings,
        secrets=dict.fromkeys(config.secrets, "***"),
        resources_configured=len(
            center.list_resources(current_user["user_id"], ns)
        ),
    )


@router.put("/{skill_name}/config/{setting_name}")
async def set_setting(
    skill_name: str,
    setting_name: str,
    body: SetSettingRequest,
    scope: ApiScope = Query("user"),
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Set a setting or secret value."""
    scope_type, scope_id = _resolve_scope(scope, current_user["user_id"], db)
    center = _get_center()
    center.set_setting(
        skill_name, setting_name, body.value,
        scope_type=scope_type, scope_id=scope_id,
        updated_by=current_user["user_id"],
    )
    return {"status": "ok"}


@router.delete("/{skill_name}/config/{setting_name}")
async def delete_setting(
    skill_name: str,
    setting_name: str,
    scope: ApiScope = Query("user"),
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Delete a setting at a specific scope."""
    scope_type, scope_id = _resolve_scope(scope, current_user["user_id"], db)
    center = _get_center()
    deleted = center.delete_setting(skill_name, setting_name, scope_type=scope_type, scope_id=scope_id)
    if not deleted:
        raise HTTPException(status_code=404, detail="Setting not found at this scope")
    return {"status": "deleted"}


# ── Resource Bindings ─────────────────────────────────────────────────────


@router.get("/{skill_name}/resources")
async def list_resources(
    skill_name: str,
    current_user: dict = Depends(get_current_user),
):
    """List all resources configured for this skill."""
    center = _get_center()
    return center.list_resources(current_user["user_id"], skill_name)


@router.put("/{skill_name}/resources/{resource_key:path}")
async def bind_resource(
    skill_name: str,
    resource_key: str,
    body: BindResourceRequest,
    current_user: dict = Depends(get_current_user),
):
    """Set/update resource bindings."""
    center = _get_center()
    center.bind_resource(current_user["user_id"], skill_name, resource_key, body.bindings)
    return {"status": "ok", "resource_key": resource_key}


@router.delete("/{skill_name}/resources/{resource_key:path}")
async def unbind_resource(
    skill_name: str,
    resource_key: str,
    current_user: dict = Depends(get_current_user),
):
    """Remove all bindings for a resource."""
    center = _get_center()
    count = center.unbind_resource(current_user["user_id"], skill_name, resource_key)
    if count == 0:
        raise HTTPException(status_code=404, detail="No bindings found for this resource")
    return {"status": "deleted", "count": count}
