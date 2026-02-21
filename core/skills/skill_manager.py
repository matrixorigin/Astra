"""Skill lifecycle manager — install, uninstall, upgrade, credential CRUD."""

from __future__ import annotations

import uuid
from datetime import datetime, timezone
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session

from api.models import (
    SkillDefinition,
    SkillInstallation,
    SkillPermission,
    UserConnection,
    UserCredential,
)
from core.skills.credential_manager import CredentialManager
from core.skills.user_db_pool import UserDBPool


class SkillNotFoundError(Exception):
    pass


class SkillNotInstalledError(Exception):
    pass


class PermissionDeniedError(Exception):
    pass


class ConnectionRequiredError(Exception):
    pass


class SkillManager:
    """Manages skill install/uninstall/upgrade lifecycle.

    Parameters
    ----------
    platform_db : Session
        Platform database session (for skill_definitions, installations, etc.)
    credential_mgr : CredentialManager
        Encryption service for user credentials.
    user_db_pool : UserDBPool
        Per-user BYOD connection pool.
    """

    # Tables created in user DB to track installed skills
    META_TABLE = "_agent_meta_installed_skills"
    META_DDL = (
        f"CREATE TABLE IF NOT EXISTS {META_TABLE} ("
        "  skill_name VARCHAR(100) PRIMARY KEY,"
        "  skill_version VARCHAR(20) NOT NULL,"
        "  installed_at DATETIME NOT NULL"
        ")"
    )

    def __init__(
        self,
        platform_db: Session,
        credential_mgr: CredentialManager,
        user_db_pool: UserDBPool,
    ):
        self._db = platform_db
        self._cred = credential_mgr
        self._pool = user_db_pool

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

    def get_connection(self, user_id: str) -> UserConnection | None:
        return self._db.query(UserConnection).filter_by(user_id=user_id, status="active").first()

    # ── permission check ──────────────────────────────────────────────────────

    def check_permission(self, user_id: str, skill_name: str) -> bool:
        """Return True if user can install this skill."""
        defn = self.get_definition(skill_name)
        if defn is None:
            return False
        if defn.is_public:
            return True
        # Check direct user grant or role-based grant
        from api.models import UserRole
        user_roles = [
            r.role_id
            for r in self._db.query(UserRole).filter_by(user_id=user_id).all()
        ]
        grant = (
            self._db.query(SkillPermission)
            .filter(
                SkillPermission.skill_name == skill_name,
            )
            .all()
        )
        for g in grant:
            if g.grantee_type == "user" and g.grantee_id == user_id:
                return True
            if g.grantee_type == "role" and g.grantee_id in user_roles:
                return True
        return False

    # ── install ───────────────────────────────────────────────────────────────

    def install(
        self,
        user_id: str,
        skill_name: str,
        *,
        table_ddls: list[str] | None = None,
    ) -> SkillInstallation:
        """Install a skill for a user.

        Parameters
        ----------
        user_id : str
        skill_name : str
        table_ddls : list[str] | None
            Platform-defined CREATE TABLE statements to run on user DB.
        """
        # 1. Skill exists?
        defn = self.get_definition(skill_name)
        if defn is None:
            raise SkillNotFoundError(f"Skill '{skill_name}' not found")

        # 2. Permission?
        if not self.check_permission(user_id, skill_name):
            raise PermissionDeniedError(f"No permission to install '{skill_name}'")

        # 3. Already installed?
        existing = self.get_installation(user_id, skill_name)
        if existing is not None:
            return existing

        # 4. BYOD connection?
        conn = self.get_connection(user_id)
        if conn is None:
            raise ConnectionRequiredError("Register a database connection first")

        # 5. Create tables on user DB
        if table_ddls:
            conn_with_pw = _ConnectionWithPassword(conn, self._cred)
            user_session = self._pool.get_session(user_id, conn_with_pw)
            try:
                # Meta table
                user_session.execute(text(self.META_DDL))
                # Skill tables
                for ddl in table_ddls:
                    user_session.execute(text(ddl))
                # Record in meta
                user_session.execute(
                    text(
                        f"INSERT INTO {self.META_TABLE} (skill_name, skill_version, installed_at) "
                        "VALUES (:name, :ver, :ts) "
                        "ON DUPLICATE KEY UPDATE skill_version = :ver, installed_at = :ts"
                    ),
                    {"name": skill_name, "ver": defn.version, "ts": _now()},
                )
                user_session.commit()
            except Exception:
                user_session.rollback()
                raise
            finally:
                user_session.close()

        # 6. Record installation in platform DB
        installation = SkillInstallation(
            installation_id=_uuid(),
            user_id=user_id,
            skill_name=skill_name,
            skill_version=defn.version,
            status="installed",
            installed_at=_now(),
        )
        self._db.add(installation)
        self._db.commit()
        return installation

    # ── uninstall ─────────────────────────────────────────────────────────────

    def uninstall(
        self,
        user_id: str,
        skill_name: str,
        *,
        drop_tables: bool = False,
        table_names: list[str] | None = None,
    ) -> None:
        """Uninstall a skill. Default keeps data; drop_tables=True removes tables."""
        inst = self.get_installation(user_id, skill_name)
        if inst is None:
            raise SkillNotInstalledError(f"'{skill_name}' is not installed")

        if drop_tables:
            if not table_names:
                raise ValueError("drop_tables=True requires table_names")
            conn = self.get_connection(user_id)
            if conn is None:
                raise ConnectionRequiredError("No database connection to drop tables from")
            conn_with_pw = _ConnectionWithPassword(conn, self._cred)
            user_session = self._pool.get_session(user_id, conn_with_pw)
            try:
                for tbl in table_names:
                    user_session.execute(text(f"DROP TABLE IF EXISTS `{tbl}`"))
                user_session.execute(
                    text(f"DELETE FROM {self.META_TABLE} WHERE skill_name = :name"),
                    {"name": skill_name},
                )
                user_session.commit()
            except Exception:
                user_session.rollback()
                raise
            finally:
                user_session.close()

        # Remove credentials
        self._db.query(UserCredential).filter_by(
            user_id=user_id, skill_name=skill_name
        ).delete()

        # Mark uninstalled
        inst.status = "uninstalled"
        inst.updated_at = _now()
        self._db.commit()

    # ── upgrade ───────────────────────────────────────────────────────────────

    def upgrade(
        self,
        user_id: str,
        skill_name: str,
        *,
        alter_ddls: list[str] | None = None,
    ) -> SkillInstallation:
        """Upgrade a skill to the latest version."""
        inst = self.get_installation(user_id, skill_name)
        if inst is None:
            raise SkillNotInstalledError(f"'{skill_name}' is not installed")

        defn = self.get_definition(skill_name)
        if defn is None:
            raise SkillNotFoundError(f"Skill '{skill_name}' not found")

        if inst.skill_version == defn.version:
            return inst  # already up to date

        # Run ALTER TABLE if needed
        if alter_ddls:
            conn = self.get_connection(user_id)
            if conn:
                conn_with_pw = _ConnectionWithPassword(conn, self._cred)
                user_session = self._pool.get_session(user_id, conn_with_pw)
                try:
                    for ddl in alter_ddls:
                        user_session.execute(text(ddl))
                    user_session.execute(
                        text(
                            f"UPDATE {self.META_TABLE} SET skill_version = :ver, installed_at = :ts "
                            "WHERE skill_name = :name"
                        ),
                        {"name": skill_name, "ver": defn.version, "ts": _now()},
                    )
                    user_session.commit()
                except Exception:
                    user_session.rollback()
                    raise
                finally:
                    user_session.close()

        inst.skill_version = defn.version
        inst.updated_at = _now()
        self._db.commit()
        return inst

    # ── credential CRUD ───────────────────────────────────────────────────────

    def save_credential(
        self, user_id: str, skill_name: str, credential_name: str, value: str
    ) -> None:
        """Encrypt and store a credential."""
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
        """Decrypt and return a credential value, or None."""
        row = (
            self._db.query(UserCredential)
            .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
            .first()
        )
        if row is None:
            return None
        return self._cred.decrypt(row.value_encrypted)

    def get_all_credentials(self, user_id: str, skill_name: str) -> dict[str, str]:
        """Return all decrypted credentials for a skill as {name: value}."""
        rows = (
            self._db.query(UserCredential)
            .filter_by(user_id=user_id, skill_name=skill_name)
            .all()
        )
        return {r.credential_name: self._cred.decrypt(r.value_encrypted) for r in rows}

    def delete_credential(self, user_id: str, skill_name: str, credential_name: str) -> bool:
        """Delete a credential. Returns True if deleted."""
        count = (
            self._db.query(UserCredential)
            .filter_by(user_id=user_id, skill_name=skill_name, credential_name=credential_name)
            .delete()
        )
        self._db.commit()
        return count > 0

    # ── connection CRUD ───────────────────────────────────────────────────────

    def register_connection(
        self,
        user_id: str,
        *,
        dialect: str,
        host: str,
        port: int,
        database: str,
        username: str,
        password: str,
    ) -> UserConnection:
        """Register or update a user's BYOD connection."""
        encrypted_pw = self._cred.encrypt(password)
        existing = self._db.query(UserConnection).filter_by(user_id=user_id).first()
        if existing:
            existing.dialect = dialect
            existing.host = host
            existing.port = port
            existing.database = database
            existing.username = username
            existing.password_encrypted = encrypted_pw
            existing.status = "active"
            existing.verified_at = None
            self._db.commit()
            return existing

        conn = UserConnection(
            connection_id=_uuid(),
            user_id=user_id,
            dialect=dialect,
            host=host,
            port=port,
            database=database,
            username=username,
            password_encrypted=encrypted_pw,
            status="active",
            created_at=_now(),
        )
        self._db.add(conn)
        self._db.commit()
        return conn

    def verify_connection(self, user_id: str) -> bool:
        """Test the user's BYOD connection. Updates verified_at on success."""
        conn = self.get_connection(user_id)
        if conn is None:
            return False
        conn_with_pw = _ConnectionWithPassword(conn, self._cred)
        ok = self._pool.test_connection(conn_with_pw)
        if ok:
            conn.verified_at = _now()
            conn.status = "active"
        else:
            conn.status = "error"
        self._db.commit()
        return ok


class _ConnectionWithPassword:
    """Adapter that decrypts password on-the-fly for UserDBPool."""

    def __init__(self, conn: UserConnection, cred: CredentialManager):
        self.dialect = conn.dialect
        self.host = conn.host
        self.port = conn.port
        self.database = conn.database
        self.username = conn.username
        self.password_decrypted = cred.decrypt(conn.password_encrypted)


def _uuid() -> str:
    return str(uuid.uuid4())


def _now() -> datetime:
    return datetime.now(timezone.utc)
