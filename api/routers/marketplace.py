"""Marketplace API Router — skill install/uninstall/upgrade + credential management."""

from fastapi import APIRouter, Depends, HTTPException, Query, status
from pydantic import BaseModel

from api.database import SessionLocal
from api.dependencies import get_current_user
from config.settings import get_settings
from core.skills.credential_manager import CredentialManager
from core.skills.resolver import CircularDependencyError, DependencyConflictError
from core.skills.skill_manager import (
    PermissionDeniedError,
    SkillManager,
    SkillNotFoundError,
    SkillNotInstalledError,
)

router = APIRouter()


# ── helpers ───────────────────────────────────────────────────────────────────

def _mgr() -> SkillManager:
    """Build SkillManager with SessionLocal factory.

    Each SkillManager method creates its own short-lived session via the
    factory, so this helper is stateless and safe to call per-request.
    """
    settings = get_settings()
    return SkillManager(SessionLocal, CredentialManager(settings.secret_key))


# ── request / response models ────────────────────────────────────────────────

class InstallRequest(BaseModel):
    skill_name: str


class CredentialRequest(BaseModel):
    skill_name: str
    credential_name: str
    value: str


class InstallationResponse(BaseModel):
    installation_id: str
    skill_name: str
    skill_version: str
    status: str


class InstalledListResponse(BaseModel):
    installations: list[InstallationResponse]


# ── skill lifecycle ──────────────────────────────────────────────────────────

@router.post("/install", response_model=InstallationResponse, status_code=status.HTTP_201_CREATED)
async def install_skill(
    req: InstallRequest,
    current_user: dict = Depends(get_current_user),
):
    """Install a skill for the current user."""
    try:
        inst = _mgr().install(current_user["user_id"], req.skill_name)
        return InstallationResponse(
            installation_id=inst.installation_id,
            skill_name=inst.skill_name,
            skill_version=inst.skill_version,
            status=inst.status,
        )
    except SkillNotFoundError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except (SkillNotInstalledError, DependencyConflictError, CircularDependencyError) as e:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail=str(e))
    except PermissionDeniedError as e:
        raise HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=str(e))


@router.post("/uninstall", status_code=status.HTTP_204_NO_CONTENT)
async def uninstall_skill(
    req: InstallRequest,
    current_user: dict = Depends(get_current_user),
):
    """Uninstall a skill (marks uninstalled + deletes credentials)."""
    try:
        _mgr().uninstall(current_user["user_id"], req.skill_name)
    except SkillNotInstalledError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except DependencyConflictError as e:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail=str(e))


@router.post("/upgrade", response_model=InstallationResponse)
async def upgrade_skill(
    req: InstallRequest,
    current_user: dict = Depends(get_current_user),
):
    """Upgrade a skill to the latest version."""
    try:
        inst = _mgr().upgrade(current_user["user_id"], req.skill_name)
        return InstallationResponse(
            installation_id=inst.installation_id,
            skill_name=inst.skill_name,
            skill_version=inst.skill_version,
            status=inst.status,
        )
    except SkillNotInstalledError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except SkillNotFoundError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except (DependencyConflictError, CircularDependencyError) as e:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail=str(e))


@router.post("/rollback", response_model=InstallationResponse)
async def rollback_skill(
    req: InstallRequest,
    current_user: dict = Depends(get_current_user),
):
    """Rollback a skill to its previous version."""
    try:
        inst = _mgr().rollback(current_user["user_id"], req.skill_name)
        return InstallationResponse(
            installation_id=inst.installation_id,
            skill_name=inst.skill_name,
            skill_version=inst.skill_version,
            status=inst.status,
        )
    except SkillNotInstalledError as e:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=str(e))
    except DependencyConflictError as e:
        raise HTTPException(status_code=status.HTTP_409_CONFLICT, detail=str(e))


@router.get("/installed", response_model=InstalledListResponse)
async def list_installed(
    current_user: dict = Depends(get_current_user),
):
    """List all installed skills for the current user."""
    rows = _mgr().list_installed(current_user["user_id"])
    return InstalledListResponse(
        installations=[
            InstallationResponse(
                installation_id=r.installation_id,
                skill_name=r.skill_name,
                skill_version=r.skill_version,
                status=r.status,
            )
            for r in rows
        ]
    )


# ── credential management ────────────────────────────────────────────────────

@router.post("/credentials", status_code=status.HTTP_204_NO_CONTENT)
async def save_credential(
    req: CredentialRequest,
    current_user: dict = Depends(get_current_user),
):
    """Save (or update) an encrypted credential for a skill."""
    _mgr().save_credential(
        current_user["user_id"], req.skill_name, req.credential_name, req.value,
    )


@router.delete("/credentials", status_code=status.HTTP_204_NO_CONTENT)
async def delete_credential(
    skill_name: str = Query(...),
    credential_name: str = Query(...),
    current_user: dict = Depends(get_current_user),
):
    """Delete a credential."""
    deleted = _mgr().delete_credential(
        current_user["user_id"], skill_name, credential_name,
    )
    if not deleted:
        raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail="Credential not found")


# ── Lifecycle transitions ─────────────────────────────────────────────────────

@router.post("/skills/{skill_name}/publish", status_code=status.HTTP_200_OK)
def publish_skill(
    skill_name: str,
    _user: dict = Depends(get_current_user),
):
    """Publish a skill: draft → active. Triggers regression gate if configured."""
    from core.skills.registry import SkillRegistry
    registry = SkillRegistry(SessionLocal)
    try:
        registry.publish(skill_name)
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_400_BAD_REQUEST, detail=str(e))
    return {"skill_name": skill_name, "status": "active"}


@router.post("/skills/{skill_name}/deprecate", status_code=status.HTTP_200_OK)
def deprecate_skill(
    skill_name: str,
    _user: dict = Depends(get_current_user),
):
    """Deprecate a skill: active → deprecated."""
    from core.skills.registry import SkillRegistry
    registry = SkillRegistry(SessionLocal)
    try:
        registry.deprecate(skill_name)
    except ValueError as e:
        raise HTTPException(status_code=status.HTTP_400_BAD_REQUEST, detail=str(e))
    return {"skill_name": skill_name, "status": "deprecated"}
