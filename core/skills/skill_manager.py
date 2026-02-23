"""Skill lifecycle manager — install, uninstall, upgrade, credential CRUD.

v3: All skill tables live in the platform DB (sk_{skill}_{table} prefix).
No BYOD, no DDL execution, no per-user connection pool.
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

from sqlalchemy.exc import IntegrityError, OperationalError
from sqlalchemy.orm import Session

from api.models import (
    SkillDefinition,
    SkillInstallation,
    SkillPermission,
    UserCredential,
)
from core.skills.credential_manager import CredentialManager


class SkillNotFoundError(Exception):
    pass


class SkillNotInstalledError(Exception):
    pass


class PermissionDeniedError(Exception):
    pass


class SkillManager:
    """Manages skill install/uninstall/upgrade lifecycle.

    Parameters
    ----------
    platform_db : Session
        Platform database session (skill tables, credentials, etc.)
    credential_mgr : CredentialManager
        Encryption service for user credentials.
    """

    def __init__(self, platform_db: Session, credential_mgr: CredentialManager):
        self._db = platform_db
        self._cred = credential_mgr

    # ── queries ───────────────────────────────────────────────────────────────

    def get_definition(self, skill_name: str) -> SkillDefinition | None:
        return (
            self._db.query(SkillDefinition)
            .filter_by(name=skill_name, is_active=1)
            .first()
        )

    def get_installation(self, user_id: str, skill_name: str) -> SkillInstallation | None:
        return (
            self._db.query(SkillInstallation)
            .filter_by(user_id=user_id, skill_name=skill_name, status="installed")
            .first()
        )

    def list_installed(self, user_id: str) -> list[SkillInstallation]:
        return (
            self._db.query(SkillInstallation)
            .filter_by(user_id=user_id, status="installed")
            .all()
        )

    # ── runtime enforcement ──────────────────────────────────────────────────

    def require_installed(self, user_id: str, skill_name: str) -> None:
        """Raise SkillNotInstalledError if skill is not installed for user."""
        if self.get_installation(user_id, skill_name) is None:
            raise SkillNotInstalledError(
                f"Skill '{skill_name}' is not installed. Run: /skill install {skill_name}"
            )

    def require_executable(self, user_id: str, skill_name: str) -> None:
        """Runtime check: installed + active + has permission + all dependencies installed.

        No-op for builtin skills (not in skill_definitions).

        Raises:
            SkillNotInstalledError: skill not installed or dependency missing
            PermissionDeniedError: user lacks permission or skill deactivated
        """
        # Single query — no is_active filter — distinguishes builtin / deactivated / active
        defn = self._db.query(SkillDefinition).filter_by(name=skill_name).first()
        if defn is None:
            return  # Builtin skill — not in catalog at all
        if not defn.is_active:
            raise PermissionDeniedError(
                f"Skill '{skill_name}' definition not found or deactivated"
            )

        if self.get_installation(user_id, skill_name) is None:
            raise SkillNotInstalledError(
                f"Skill '{skill_name}' is not installed. Run: /skill install {skill_name}"
            )

        if not self.check_permission(user_id, skill_name, _defn=defn):
            raise PermissionDeniedError(
                f"Permission to execute '{skill_name}' has been revoked"
            )

        # Check direct dependencies
        if defn.manifest:
            for dep in defn.manifest.get("depends_on", []):
                if self.get_installation(user_id, dep) is None:
                    raise SkillNotInstalledError(
                        f"Dependency '{dep}' required by '{skill_name}' is not installed"
                    )

    # ── permission check ──────────────────────────────────────────────────────

    def check_permission(
        self, user_id: str, skill_name: str, *, _defn: "SkillDefinition | None" = None
    ) -> bool:
        """Return True if user can install this skill."""
        defn = _defn or self.get_definition(skill_name)
        if defn is None:
            return False
        if defn.is_public:
            return True
        from api.models import UserRole
        user_roles = [
            r.role_id
            for r in self._db.query(UserRole).filter_by(user_id=user_id).all()
        ]
        for g in self._db.query(SkillPermission).filter_by(skill_name=skill_name).all():
            if g.grantee_type == "user" and g.grantee_id == user_id:
                return True
            if g.grantee_type == "role" and g.grantee_id in user_roles:
                return True
        return False

    # ── install ───────────────────────────────────────────────────────────────

    def install(self, user_id: str, skill_name: str) -> SkillInstallation:
        """Install a skill for a user (record only, no DDL)."""
        defn = self.get_definition(skill_name)
        if defn is None:
            raise SkillNotFoundError(f"Skill '{skill_name}' not found")
        if not self.check_permission(user_id, skill_name):
            raise PermissionDeniedError(f"No permission to install '{skill_name}'")

        # Check dependencies
        manifest = defn.manifest or {}
        for dep in manifest.get("depends_on", []):
            if self.get_installation(user_id, dep) is None:
                raise SkillNotInstalledError(
                    f"Dependency '{dep}' must be installed before '{skill_name}'"
                )

        existing = self.get_installation(user_id, skill_name)
        if existing is not None:
            return existing
        
        installation = SkillInstallation(
            installation_id=_uuid(),
            user_id=user_id,
            skill_name=skill_name,
            skill_version=defn.version,
            status="installed",
            installed_at=_now(),
        )
        try:
            self._db.add(installation)
            self._db.commit()
        except IntegrityError:
            self._db.rollback()
            result = self.get_installation(user_id, skill_name)
            assert result is not None, f"Installation vanished after IntegrityError: {user_id}/{skill_name}"
            return result
        except OperationalError as e:
            self._db.rollback()
            if getattr(e.orig, "args", (None,))[0] == 20619:
                # MatrixOne w-w conflict — concurrent insert won
                result = self.get_installation(user_id, skill_name)
                assert result is not None, f"Installation vanished after w-w conflict: {user_id}/{skill_name}"
                return result
            raise
        return installation

    # ── uninstall ─────────────────────────────────────────────────────────────

    def uninstall(self, user_id: str, skill_name: str) -> None:
        """Uninstall a skill: mark uninstalled + delete credentials."""
        inst = self.get_installation(user_id, skill_name)
        if inst is None:
            raise SkillNotInstalledError(f"'{skill_name}' is not installed")
        self._db.query(UserCredential).filter_by(
            user_id=user_id, skill_name=skill_name
        ).delete()
        inst.status = "uninstalled"
        inst.updated_at = _now()
        self._db.commit()

    # ── upgrade ───────────────────────────────────────────────────────────────

    def upgrade(self, user_id: str, skill_name: str) -> SkillInstallation:
        """Upgrade a skill to the latest version (version bump only)."""
        inst = self.get_installation(user_id, skill_name)
        if inst is None:
            raise SkillNotInstalledError(f"'{skill_name}' is not installed")
        defn = self.get_definition(skill_name)
        if defn is None:
            raise SkillNotFoundError(f"Skill '{skill_name}' not found")
        if inst.skill_version == defn.version:
            return inst
        inst.skill_version = defn.version
        inst.updated_at = _now()
        self._db.commit()
        return inst

    # ── credential CRUD ───────────────────────────────────────────────────────

    def save_credential(
        self, user_id: str, skill_name: str, credential_name: str, value: str
    ) -> None:
        encrypted = self._cred.encrypt(value)
        existing = (
            self._db.query(UserCredential)
            .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
            .first()
        )
        if existing:
            existing.value_encrypted = encrypted
            existing.rotated_at = _now()
        else:
            self._db.add(
                UserCredential(
                    credential_id=_uuid(),
                    user_id=user_id,
                    skill_name=skill_name,
                    credential_name=credential_name,
                    value_encrypted=encrypted,
                    created_at=_now(),
                )
            )
        self._db.commit()

    def get_credential(self, user_id: str, skill_name: str, credential_name: str) -> str | None:
        row = (
            self._db.query(UserCredential)
            .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
            .first()
        )
        if row is None:
            return None
        return self._cred.decrypt(row.value_encrypted)

    def get_all_credentials(self, user_id: str, skill_name: str) -> dict[str, str]:
        rows = (
            self._db.query(UserCredential)
            .filter_by(user_id=user_id, skill_name=skill_name)
            .all()
        )
        return {r.credential_name: self._cred.decrypt(r.value_encrypted) for r in rows}

    def delete_credential(self, user_id: str, skill_name: str, credential_name: str) -> bool:
        count = (
            self._db.query(UserCredential)
            .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
            .delete()
        )
        self._db.commit()
        return count > 0


def _uuid() -> str:
    return str(uuid.uuid4())


def _now() -> datetime:
    return datetime.now(timezone.utc)
