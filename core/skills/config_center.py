"""Skill Configuration Center — unified settings, secrets, and resource bindings.

Design doc: docs/design/skills-and-tools.md §13
"""

from __future__ import annotations

import base64
import hashlib
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Literal

from uuid_utils import uuid7

from core.db_consumer import DbConsumer, DbFactory
from core.logging_config import get_logger

if TYPE_CHECKING:
    from collections.abc import Callable

logger = get_logger(__name__)


class CredentialManager:
    """Encrypt/decrypt skill credentials using Fernet (AES-128-CBC)."""

    def __init__(self, secret_key: str):
        from cryptography.fernet import Fernet
        key = hashlib.sha256(secret_key.encode()).digest()
        self._fernet = Fernet(base64.urlsafe_b64encode(key))

    def encrypt(self, plaintext: str) -> str:
        return self._fernet.encrypt(plaintext.encode()).decode()

    def decrypt(self, ciphertext: str) -> str:
        return self._fernet.decrypt(ciphertext.encode()).decode()

ScopeType = Literal["user", "tenant", "global"]
_SCOPE_CHAIN: list[ScopeType] = ["user", "tenant", "global"]
_VALID_SCOPES: frozenset[str] = frozenset(_SCOPE_CHAIN)


@dataclass
class SkillConfig:
    """Resolved configuration passed to skill at execution time."""

    settings: dict[str, Any] = field(default_factory=dict)
    secrets: dict[str, str] = field(default_factory=dict)
    resource: dict[str, Any] | None = None
    resource_type: str | None = None
    resource_key: str | None = None


@dataclass
class ConfigValidationError:
    """A missing or invalid configuration item."""

    section: str  # "settings" | "secrets" | "resources"
    name: str
    resource_key: str | None = None
    error: str = ""


class SkillConfigCenter(DbConsumer):
    """Unified configuration center for skills.

    Handles settings (plaintext), secrets (encrypted), and resource bindings.

    Note on set/get asymmetry:
      set_setting takes explicit (scope_type, scope_id) — caller chooses WHERE to write.
      get_setting takes (user_id, tenant_id) — resolves the full scope chain automatically.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        credential_mgr: CredentialManager,
        manifest_loader: Callable[[str], dict | None] | None = None,
    ):
        super().__init__(db_factory)
        self._cred = credential_mgr
        self._manifest_loader = manifest_loader or (lambda _: None)

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _validate_scope(scope_type: str, scope_id: str | None) -> None:
        if scope_type not in _VALID_SCOPES:
            raise ValueError(f"Invalid scope_type: {scope_type!r}. Must be one of {_VALID_SCOPES}")
        if scope_type in ("user", "tenant") and not scope_id:
            raise ValueError(f"scope_id is required for scope_type={scope_type!r}")

    def _load_manifest(self, skill_name: str) -> dict:
        return self._manifest_loader(skill_name) or {}

    def get_manifest(self, skill_name: str) -> dict | None:
        """Return the manifest for a skill, or None if not found.

        Public API — use this instead of _load_manifest() from outside the class.
        Returns None (not {}) so callers can distinguish "not found" from "empty manifest".
        """
        return self._manifest_loader(skill_name) or None

    def _manifest_settings(self, manifest: dict) -> list[dict]:
        return manifest.get("settings", [])

    def _manifest_secrets(self, manifest: dict) -> list[dict]:
        return manifest.get("secrets", [])

    def _manifest_resources(self, manifest: dict) -> dict:
        return manifest.get("resources", {})

    def _is_secret_name(self, skill_name: str, setting_name: str) -> bool:
        """Check if a setting_name is declared as a secret in the manifest."""
        manifest = self._load_manifest(skill_name)
        return any(s.get("name") == setting_name for s in self._manifest_secrets(manifest))

    def _manifest_default(self, skill_name: str, setting_name: str) -> Any:
        """Return manifest default for a setting/secret, or _MISSING."""
        manifest = self._load_manifest(skill_name)
        for s in self._manifest_settings(manifest) + self._manifest_secrets(manifest):
            if s.get("name") == setting_name and "default" in s:
                return s["default"]
        return None

    # ------------------------------------------------------------------
    # Settings & Secrets (scoped)
    # ------------------------------------------------------------------

    def set_setting(
        self,
        skill_name: str,
        setting_name: str,
        value: Any,
        scope_type: ScopeType = "user",
        scope_id: str | None = None,
        updated_by: str = "",
    ) -> None:
        """Set a setting or secret at a specific scope level."""
        from api.models.skill import SkillSetting

        self._validate_scope(scope_type, scope_id)

        is_secret = 1 if self._is_secret_name(skill_name, setting_name) else 0
        stored_value = self._cred.encrypt(str(value)) if is_secret else str(value)

        with self._db() as db:
            existing = db.query(SkillSetting).filter(
                SkillSetting.skill_name == skill_name,
                SkillSetting.setting_name == setting_name,
                SkillSetting.scope_type == scope_type,
                SkillSetting.scope_id == scope_id,
            ).first()

            if existing:
                existing.setting_value = stored_value
                existing.is_secret = is_secret
                existing.updated_by = updated_by
            else:
                db.add(SkillSetting(
                    setting_id=str(uuid7()),
                    skill_name=skill_name,
                    setting_name=setting_name,
                    setting_value=stored_value,
                    is_secret=is_secret,
                    scope_type=scope_type,
                    scope_id=scope_id,
                    updated_by=updated_by,
                ))
            db.commit()

    def get_setting(
        self,
        skill_name: str,
        setting_name: str,
        user_id: str,
        tenant_id: str | None = None,
    ) -> Any:
        """Resolve effective setting value: user → tenant → global → manifest default."""
        from api.models.skill import SkillSetting

        with self._db() as db:
            scopes: list[tuple[str, str | None]] = [("user", user_id)]
            if tenant_id:
                scopes.append(("tenant", tenant_id))
            scopes.append(("global", None))

            for scope_type, scope_id in scopes:
                row = db.query(SkillSetting).filter(
                    SkillSetting.skill_name == skill_name,
                    SkillSetting.setting_name == setting_name,
                    SkillSetting.scope_type == scope_type,
                    SkillSetting.scope_id == scope_id,
                ).first()
                if row:
                    return self._cred.decrypt(row.setting_value) if row.is_secret else row.setting_value

        return self._manifest_default(skill_name, setting_name)

    def delete_setting(
        self,
        skill_name: str,
        setting_name: str,
        scope_type: ScopeType = "user",
        scope_id: str | None = None,
    ) -> bool:
        """Delete a setting at a specific scope. Returns True if deleted."""
        from api.models.skill import SkillSetting

        self._validate_scope(scope_type, scope_id)

        with self._db() as db:
            count = db.query(SkillSetting).filter(
                SkillSetting.skill_name == skill_name,
                SkillSetting.setting_name == setting_name,
                SkillSetting.scope_type == scope_type,
                SkillSetting.scope_id == scope_id,
            ).delete(synchronize_session=False)
            db.commit()
            return count > 0

    # ------------------------------------------------------------------
    # Resource Bindings
    # ------------------------------------------------------------------

    def bind_resource(
        self,
        user_id: str,
        skill_name: str,
        resource_key: str,
        bindings: dict[str, Any],
    ) -> None:
        """Bind credentials/config to a specific resource instance."""
        from api.models.skill import SkillResourceBinding

        manifest = self._load_manifest(skill_name)
        res = self._manifest_resources(manifest)
        resource_type = res.get("type", "unknown")
        binding_defs = {b["name"]: b for b in res.get("bindings", [])}

        with self._db() as db:
            for name, value in bindings.items():
                bdef = binding_defs.get(name, {})
                is_secret = 1 if bdef.get("type") == "secret" else 0
                stored = self._cred.encrypt(str(value)) if is_secret else str(value)

                existing = db.query(SkillResourceBinding).filter(
                    SkillResourceBinding.user_id == user_id,
                    SkillResourceBinding.skill_name == skill_name,
                    SkillResourceBinding.resource_key == resource_key,
                    SkillResourceBinding.binding_name == name,
                ).first()

                if existing:
                    existing.binding_value = stored
                    existing.is_secret = is_secret
                    existing.updated_by = user_id
                else:
                    db.add(SkillResourceBinding(
                        binding_id=str(uuid7()),
                        user_id=user_id,
                        skill_name=skill_name,
                        resource_type=resource_type,
                        resource_key=resource_key,
                        binding_name=name,
                        binding_value=stored,
                        is_secret=is_secret,
                        updated_by=user_id,
                    ))
            db.commit()

    def get_resource_binding(
        self,
        user_id: str,
        skill_name: str,
        resource_key: str,
        binding_name: str,
    ) -> Any:
        """Get a specific resource binding, falling back to skill-level secret."""
        from api.models.skill import SkillResourceBinding

        with self._db() as db:
            row = db.query(SkillResourceBinding).filter(
                SkillResourceBinding.user_id == user_id,
                SkillResourceBinding.skill_name == skill_name,
                SkillResourceBinding.resource_key == resource_key,
                SkillResourceBinding.binding_name == binding_name,
            ).first()
            if row:
                return self._cred.decrypt(row.binding_value) if row.is_secret else row.binding_value

        # Fallback to skill-level secret with same name
        return self.get_setting(skill_name, binding_name, user_id)

    def unbind_resource(
        self,
        user_id: str,
        skill_name: str,
        resource_key: str,
    ) -> int:
        """Remove all bindings for a resource. Returns count deleted."""
        from api.models.skill import SkillResourceBinding

        with self._db() as db:
            count = db.query(SkillResourceBinding).filter(
                SkillResourceBinding.user_id == user_id,
                SkillResourceBinding.skill_name == skill_name,
                SkillResourceBinding.resource_key == resource_key,
            ).delete(synchronize_session=False)
            db.commit()
            return count

    def list_resources(
        self,
        user_id: str,
        skill_name: str,
    ) -> list[dict[str, Any]]:
        """List all resources the user has configured for a skill."""
        from api.models.skill import SkillResourceBinding

        with self._db() as db:
            rows = db.query(
                SkillResourceBinding.resource_key,
                SkillResourceBinding.resource_type,
            ).filter(
                SkillResourceBinding.user_id == user_id,
                SkillResourceBinding.skill_name == skill_name,
            ).group_by(
                SkillResourceBinding.resource_key,
                SkillResourceBinding.resource_type,
            ).all()
            return [{"resource_key": r[0], "resource_type": r[1]} for r in rows]

    # ------------------------------------------------------------------
    # Bulk Resolution
    # ------------------------------------------------------------------

    def resolve_all(
        self,
        skill_name: str,
        user_id: str,
        tenant_id: str | None = None,
        resource_key: str | None = None,
    ) -> SkillConfig:
        """Resolve complete effective configuration for skill execution.

        Uses a single DB session for all queries to avoid N+1 session overhead.
        """
        from api.models.skill import SkillResourceBinding, SkillSetting

        manifest = self._load_manifest(skill_name)

        with self._db() as db:
            # Build scope chain once
            scopes: list[tuple[str, str | None]] = [("user", user_id)]
            if tenant_id:
                scopes.append(("tenant", tenant_id))
            scopes.append(("global", None))

            # Batch-load all settings for this skill in one query
            all_rows = db.query(SkillSetting).filter(
                SkillSetting.skill_name == skill_name,
            ).all()

            # Index by (setting_name, scope_type, scope_id)
            row_index: dict[tuple[str, str, str | None], SkillSetting] = {}
            for row in all_rows:
                row_index[(row.setting_name, row.scope_type, row.scope_id)] = row

            def _resolve(name: str) -> Any:
                for scope_type, scope_id in scopes:
                    row = row_index.get((name, scope_type, scope_id))
                    if row:
                        return self._cred.decrypt(row.setting_value) if row.is_secret else row.setting_value
                return None

            # Settings
            settings: dict[str, Any] = {}
            for s in self._manifest_settings(manifest):
                name = s["name"]
                val = _resolve(name)
                if val is None:
                    val = s.get("default")
                if val is not None:
                    settings[name] = val

            # Secrets
            secrets: dict[str, str] = {}
            for s in self._manifest_secrets(manifest):
                name = s["name"]
                val = _resolve(name)
                if val is None:
                    val = s.get("default")
                if val is not None:
                    secrets[name] = val

            # Resource bindings
            resource: dict[str, Any] | None = None
            resource_type: str | None = None
            res_spec = self._manifest_resources(manifest)
            if resource_key and res_spec:
                resource_type = res_spec.get("type")
                resource = {}

                # Batch-load all bindings for this user+skill+resource in one query
                binding_rows = db.query(SkillResourceBinding).filter(
                    SkillResourceBinding.user_id == user_id,
                    SkillResourceBinding.skill_name == skill_name,
                    SkillResourceBinding.resource_key == resource_key,
                ).all()
                binding_index = {r.binding_name: r for r in binding_rows}

                for bdef in res_spec.get("bindings", []):
                    bname = bdef["name"]
                    brow = binding_index.get(bname)
                    if brow:
                        val = self._cred.decrypt(brow.binding_value) if brow.is_secret else brow.binding_value
                    else:
                        # Fallback to skill-level setting
                        val = _resolve(bname)
                    if val is None:
                        val = bdef.get("default")
                    if val is not None:
                        resource[bname] = val

        return SkillConfig(
            settings=settings,
            secrets=secrets,
            resource=resource,
            resource_type=resource_type,
            resource_key=resource_key if resource is not None else None,
        )

    # ------------------------------------------------------------------
    # Validation
    # ------------------------------------------------------------------

    def validate(
        self,
        skill_name: str,
        user_id: str,
        tenant_id: str | None = None,
        resource_key: str | None = None,
    ) -> list[ConfigValidationError]:
        """Validate all required config is present. Returns empty list if valid."""
        from api.models.skill import SkillResourceBinding, SkillSetting

        manifest = self._load_manifest(skill_name)
        errors: list[ConfigValidationError] = []

        scopes: list[tuple[str, str | None]] = [("user", user_id)]
        if tenant_id:
            scopes.append(("tenant", tenant_id))
        scopes.append(("global", None))

        with self._db() as db:
            # Batch-load only settings relevant to this user's scope hierarchy
            from sqlalchemy import or_
            scope_ids = [s for _, s in scopes if s is not None]
            all_rows = db.query(SkillSetting).filter(
                SkillSetting.skill_name == skill_name,
                or_(
                    SkillSetting.scope_type == "global",
                    SkillSetting.scope_id.in_(scope_ids),
                ),
            ).all()
            row_index: dict[tuple[str, str, str | None], SkillSetting] = {
                (r.setting_name, r.scope_type, r.scope_id): r for r in all_rows
            }

            def _resolve(name: str) -> Any:
                for scope_type, scope_id in scopes:
                    row = row_index.get((name, scope_type, scope_id))
                    if row:
                        return self._cred.decrypt(row.setting_value) if row.is_secret else row.setting_value
                return None

            for s in self._manifest_settings(manifest):
                if s.get("required") and "default" not in s:
                    if _resolve(s["name"]) is None:
                        errors.append(ConfigValidationError("settings", s["name"], error="required but not set"))

            for s in self._manifest_secrets(manifest):
                if s.get("required") and "default" not in s:
                    if _resolve(s["name"]) is None:
                        errors.append(ConfigValidationError("secrets", s["name"], error="required but not set"))

            res_spec = self._manifest_resources(manifest)
            if resource_key and res_spec:
                binding_rows = db.query(SkillResourceBinding).filter(
                    SkillResourceBinding.user_id == user_id,
                    SkillResourceBinding.skill_name == skill_name,
                    SkillResourceBinding.resource_key == resource_key,
                ).all()
                binding_index = {r.binding_name: r for r in binding_rows}

                for bdef in res_spec.get("bindings", []):
                    if bdef.get("required") and "default" not in bdef:
                        brow = binding_index.get(bdef["name"])
                        if brow is None and _resolve(bdef["name"]) is None:
                            errors.append(ConfigValidationError(
                                "resources", bdef["name"],
                                resource_key=resource_key,
                                error="required but not set",
                            ))

        return errors


# ---------------------------------------------------------------------------
# Singleton factory — use this instead of importing from api.routers
# ---------------------------------------------------------------------------

_shared_center: "SkillConfigCenter | None" = None


def init_config_center(
    db_factory: "DbFactory",
    credential_mgr: "CredentialManager",
    manifest_loader: "Callable[[str], dict | None]",
) -> "SkillConfigCenter":
    """Initialize the shared SkillConfigCenter singleton.

    Must be called once at application startup (from api/ layer, which owns
    SessionLocal and the ORM models). After initialization, core/ modules call
    get_config_center() without any api/ imports.

    This is the correct fix for the core/ → api/ layering problem: the api/
    layer owns the DB session factory and passes it down, rather than core/
    reaching up to import api/.
    """
    global _shared_center
    _shared_center = SkillConfigCenter(db_factory, credential_mgr, manifest_loader)
    return _shared_center


def get_config_center() -> "SkillConfigCenter":
    """Return the shared SkillConfigCenter singleton.

    Raises RuntimeError if init_config_center() has not been called yet.
    In tests, call init_config_center() in a fixture or use the test-injection
    point in api/routers/skill_config.py (_center override).
    """
    if _shared_center is None:
        raise RuntimeError(
            "SkillConfigCenter not initialized. "
            "Call init_config_center() at application startup."
        )
    return _shared_center


def _reset_config_center_for_tests() -> None:
    """Reset the singleton — for use in test teardown only."""
    global _shared_center
    _shared_center = None
