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
    SkillInstallation,
    SkillPermission,
    SkillRegistry,
    UserCredential,
    UserRole,
)
from core.db_consumer import DbConsumer
from core.skills.credential_manager import CredentialManager


class SkillNotFoundError(Exception):
    pass


class SkillNotInstalledError(Exception):
    pass


class PermissionDeniedError(Exception):
    pass


class SkillManager(DbConsumer):
    """Manages skill install/uninstall/upgrade lifecycle.

    Inherits DbConsumer for consistent session management:
    ``with self._db() as db:`` creates a fresh session per operation,
    auto-rollbacks on exception, and always closes on exit.

    Parameters
    ----------
    db_factory : Callable[[], Session]
        Factory that returns a new database session.
    credential_mgr : CredentialManager
        Encryption service for user credentials.
    """

    def __init__(self, db_factory, credential_mgr: CredentialManager):
        super().__init__(db_factory)
        self._cred = credential_mgr

    # ── internal queries (accept db to avoid nested sessions) ────────────────

    def _get_definition(self, db: Session, skill_name: str) -> SkillRegistry | None:
        return db.query(SkillRegistry).filter_by(skill_name=skill_name, is_active=1).first()

    def _get_installation(self, db: Session, user_id: str, skill_name: str) -> SkillInstallation | None:
        return db.query(SkillInstallation).filter_by(user_id=user_id, skill_name=skill_name, status="installed").first()

    # ── public queries ───────────────────────────────────────────────────────
    # Returned ORM objects are expunged from the session so callers can safely
    # access any loaded attribute after the session is closed.

    def get_definition(self, skill_name: str) -> SkillRegistry | None:
        with self._db() as db:
            row = self._get_definition(db, skill_name)
            if row is not None:
                db.expunge(row)
            return row

    def get_installation(self, user_id: str, skill_name: str) -> SkillInstallation | None:
        with self._db() as db:
            row = self._get_installation(db, user_id, skill_name)
            if row is not None:
                db.expunge(row)
            return row

    def list_installed(self, user_id: str) -> list[SkillInstallation]:
        with self._db() as db:
            rows = db.query(SkillInstallation).filter_by(user_id=user_id, status="installed").all()
            for r in rows:
                db.expunge(r)
            return rows

    # ── runtime enforcement ──────────────────────────────────────────────────

    def require_installed(self, user_id: str, skill_name: str) -> None:
        """Raise SkillNotInstalledError if skill is not installed for user."""
        with self._db() as db:
            if self._get_installation(db, user_id, skill_name) is None:
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
        with self._db() as db:
            defn = db.query(SkillRegistry).filter_by(skill_name=skill_name).first()
            if defn is None:
                return  # Builtin skill — not in catalog at all
            if getattr(defn, "source", "builtin") == "builtin":
                return  # Builtin skills skip install/permission checks
            status = getattr(defn, "status", "active") or "active"
            if status != "active":
                raise PermissionDeniedError(
                    f"Skill '{skill_name}' is in '{status}' state — only 'active' skills can execute"
                )
            if not defn.is_active:
                raise PermissionDeniedError(
                    f"Skill '{skill_name}' definition not found or deactivated"
                )

            if self._get_installation(db, user_id, skill_name) is None:
                raise SkillNotInstalledError(
                    f"Skill '{skill_name}' is not installed. Run: /skill install {skill_name}"
                )

            if not self._check_permission(db, user_id, skill_name, _defn=defn):
                raise PermissionDeniedError(
                    f"Permission to execute '{skill_name}' has been revoked"
                )

            # Check direct dependencies
            if defn.manifest:
                for dep in defn.manifest.get("depends_on", []):
                    if self._get_installation(db, user_id, dep) is None:
                        raise SkillNotInstalledError(
                            f"Dependency '{dep}' required by '{skill_name}' is not installed"
                        )

    # ── permission check ──────────────────────────────────────────────────────

    def _check_permission(
        self, db: Session, user_id: str, skill_name: str, *, _defn: SkillRegistry | None = None
    ) -> bool:
        defn = _defn or self._get_definition(db, skill_name)
        if defn is None:
            return False
        if defn.is_public:
            return True
        user_roles = [r.role_id for r in db.query(UserRole).filter_by(user_id=user_id).all()]
        for g in db.query(SkillPermission).filter_by(skill_name=skill_name).all():
            if g.grantee_type == "user" and g.grantee_id == user_id:
                return True
            if g.grantee_type == "role" and g.grantee_id in user_roles:
                return True
        return False

    def check_permission(self, user_id: str, skill_name: str) -> bool:
        """Return True if user can install this skill.

        Note: _defn is intentionally not exposed in the public API to avoid
        passing detached ORM objects across session boundaries.  Internal
        callers that already hold a session use _check_permission() directly.
        """
        with self._db() as db:
            return self._check_permission(db, user_id, skill_name)

    # ── install ───────────────────────────────────────────────────────────────

    def install(self, user_id: str, skill_name: str) -> SkillInstallation:
        """Install a skill for a user (record only, no DDL)."""
        with self._db() as db:
            defn = self._get_definition(db, skill_name)
            if defn is None:
                raise SkillNotFoundError(f"Skill '{skill_name}' not found")
            if not self._check_permission(db, user_id, skill_name):
                raise PermissionDeniedError(f"No permission to install '{skill_name}'")

            # Check dependencies
            manifest = defn.manifest or {}
            for dep in manifest.get("depends_on", []):
                if self._get_installation(db, user_id, dep) is None:
                    raise SkillNotInstalledError(
                        f"Dependency '{dep}' must be installed before '{skill_name}'"
                    )

            existing = self._get_installation(db, user_id, skill_name)
            if existing is not None:
                db.expunge(existing)
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
                db.add(installation)
                db.commit()
                db.refresh(installation)
            except IntegrityError:
                db.rollback()
                result = self._get_installation(db, user_id, skill_name)
                if result is None:
                    raise SkillNotFoundError(
                        f"Installation vanished after IntegrityError: {user_id}/{skill_name}"
                    )
                db.expunge(result)
                return result
            except OperationalError as e:
                db.rollback()
                if getattr(e.orig, "args", (None,))[0] == 20619:
                    # MatrixOne w-w conflict — concurrent insert won
                    result = self._get_installation(db, user_id, skill_name)
                    if result is None:
                        raise SkillNotFoundError(
                            f"Installation vanished after w-w conflict: {user_id}/{skill_name}"
                        )
                    db.expunge(result)
                    return result
                raise
            db.expunge(installation)
            return installation

    # ── uninstall ─────────────────────────────────────────────────────────────

    def uninstall(self, user_id: str, skill_name: str) -> None:
        """Uninstall a skill: mark uninstalled + delete credentials."""
        with self._db() as db:
            inst = self._get_installation(db, user_id, skill_name)
            if inst is None:
                raise SkillNotInstalledError(f"'{skill_name}' is not installed")
            db.query(UserCredential).filter_by(
                user_id=user_id, skill_name=skill_name
            ).delete()
            inst.status = "uninstalled"
            inst.updated_at = _now()
            db.commit()

    # ── upgrade ───────────────────────────────────────────────────────────────

    def upgrade(self, user_id: str, skill_name: str) -> SkillInstallation:
        """Upgrade a skill to the latest version (version bump only)."""
        with self._db() as db:
            inst = self._get_installation(db, user_id, skill_name)
            if inst is None:
                raise SkillNotInstalledError(f"'{skill_name}' is not installed")
            defn = self._get_definition(db, skill_name)
            if defn is None:
                raise SkillNotFoundError(f"Skill '{skill_name}' not found")
            if inst.skill_version != defn.version:
                inst.previous_version = inst.skill_version
                inst.skill_version = defn.version
                inst.updated_at = _now()
                db.commit()
                db.refresh(inst)
            db.expunge(inst)
            return inst

    def rollback(self, user_id: str, skill_name: str) -> SkillInstallation:
        """Rollback a skill to its previous version.

        Raises SkillNotInstalledError if not installed or no previous version.
        """
        with self._db() as db:
            inst = self._get_installation(db, user_id, skill_name)
            if inst is None:
                raise SkillNotInstalledError(f"'{skill_name}' is not installed")
            prev = getattr(inst, "previous_version", None)
            if not prev:
                raise SkillNotInstalledError(f"'{skill_name}' has no previous version to rollback to")
            inst.skill_version, inst.previous_version = prev, inst.skill_version
            inst.updated_at = _now()
            db.commit()
            db.refresh(inst)
            db.expunge(inst)
            return inst

    # ── credential CRUD ───────────────────────────────────────────────────────

    def save_credential(
        self, user_id: str, skill_name: str, credential_name: str, value: str
    ) -> None:
        encrypted = self._cred.encrypt(value)
        with self._db() as db:
            existing = (
                db.query(UserCredential)
                .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
                .first()
            )
            if existing:
                existing.value_encrypted = encrypted
                existing.rotated_at = _now()
            else:
                db.add(
                    UserCredential(
                        credential_id=_uuid(),
                        user_id=user_id,
                        skill_name=skill_name,
                        credential_name=credential_name,
                        value_encrypted=encrypted,
                        created_at=_now(),
                    )
                )
            db.commit()

    def get_credential(self, user_id: str, skill_name: str, credential_name: str) -> str | None:
        with self._db() as db:
            row = (
                db.query(UserCredential)
                .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
                .first()
            )
            if row is None:
                return None
            return self._cred.decrypt(row.value_encrypted)

    def get_all_credentials(self, user_id: str, skill_name: str) -> dict[str, str]:
        with self._db() as db:
            rows = (
                db.query(UserCredential)
                .filter_by(user_id=user_id, skill_name=skill_name)
                .all()
            )
            return {r.credential_name: self._cred.decrypt(r.value_encrypted) for r in rows}

    def delete_credential(self, user_id: str, skill_name: str, credential_name: str) -> bool:
        with self._db() as db:
            count = (
                db.query(UserCredential)
                .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
                .delete()
            )
            db.commit()
            return count > 0


def _uuid() -> str:
    return str(uuid.uuid4())


def _now() -> datetime:
    return datetime.now(timezone.utc)
