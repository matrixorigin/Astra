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
from core.skills.config_center import SkillConfigCenter
from core.skills.credential_manager import CredentialManager

if TYPE_CHECKING:
    from sqlalchemy.orm import Session

router = APIRouter()

# Module-level singleton — same pattern as skills.py _catalog_instance.
_center: SkillConfigCenter | None = None


def _get_center() -> SkillConfigCenter:
    global _center
    if _center is None:
        import os

        from api.models.skill import SkillRegistry

        key = os.environ.get("TOKEN_ENCRYPTION_KEY", "dev-key")
        cred_mgr = CredentialManager(key)

        def _manifest_loader(skill_name: str) -> dict | None:
            db = SessionLocal()
            try:
                row = db.query(SkillRegistry.manifest).filter(
                    SkillRegistry.skill_name == skill_name,
                    SkillRegistry.is_active == 1,
                ).order_by(SkillRegistry.created_at.desc()).first()
                return row[0] if row and row[0] else None
            finally:
                db.close()

        _center = SkillConfigCenter(SessionLocal, cred_mgr, _manifest_loader)
    return _center


# Only "user" and "global" scopes are supported.
# Tenant scope is reserved — see module docstring.
ApiScope = Literal["user", "global"]


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
    errors = center.validate(skill_name, current_user["user_id"], resource_key=resource)
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
    config = center.resolve_all(skill_name, current_user["user_id"])
    return ConfigResponse(
        settings=config.settings,
        secrets=dict.fromkeys(config.secrets, "***"),
        resources_configured=len(
            center.list_resources(current_user["user_id"], skill_name)
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
