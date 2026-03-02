"""SkillCatalog — single source of truth for skill CRUD, versioning, and lifecycle.

Consolidates the old SkillRegistry + SkillService into one class.
Handles: register, update, publish, unpublish, deprecate, rollback, delete,
conflict detection, source filtering, and historical queries.
"""

from __future__ import annotations

import hashlib
import inspect
from datetime import datetime
from typing import TYPE_CHECKING, Any

from sqlalchemy.orm import Session

from api.models import SkillRegistry as SkillModel
from core.db_consumer import DbConsumer, DbFactory
from core.exceptions import SkillNotFoundError
from core.logging_config import get_logger

from .base import Skill

if TYPE_CHECKING:
    from core.evaluation.gate_trigger import GateTrigger

logger = get_logger(__name__)

# Valid source values
SOURCE_BUILTIN = "builtin"
SOURCE_MARKETPLACE = "marketplace"
SOURCE_USER = "user"
_VALID_SOURCES = {SOURCE_BUILTIN, SOURCE_MARKETPLACE, SOURCE_USER}

# Valid status values
_VALID_STATUSES = {"draft", "active", "deprecated", "archived"}

# Valid status transitions
_STATUS_TRANSITIONS: dict[str, set[str]] = {
    "draft": {"active"},
    "active": {"deprecated"},
    "deprecated": {"archived", "active"},
    "archived": set(),
}


_CACHE_MISS = object()  # sentinel for "queried DB, skill not found"

# Access level ordering for list_available() repo-type filtering
_ACCESS_ORDER = {"read": 0, "write": 1, "admin": 2}


class NameConflictError(Exception):
    """Raised when a skill name conflicts with an existing skill."""

    def __init__(self, name: str, reason: str):
        self.name = name
        super().__init__(f"Name conflict for '{name}': {reason}")


class SkillCatalog(DbConsumer):
    """Single source of truth for skill metadata and lifecycle.

    Intended to be used as a **singleton** — the in-memory ``_skills`` dict
    and ``_metadata_cache`` only provide value when the same instance is
    reused across requests.  Create one instance at startup and share it
    (e.g. via FastAPI dependency injection or module-level variable).
    """

    def __init__(
        self,
        db_factory: DbFactory,
        gate_trigger: GateTrigger | None = None,
        embed_fn=None,
    ):
        super().__init__(db_factory)
        self.gate_trigger = gate_trigger
        # Resolve embed_fn: explicit > EmbeddingClient singleton > None
        if embed_fn is None:
            try:
                from core.context.embeddings import get_embedding_client
                embed_fn = get_embedding_client().embed
            except Exception:
                pass
        self._embed_fn = embed_fn
        self._skills: dict[str, Skill] = {}  # name@version or name -> Skill
        # Manual metadata cache — avoids the memory-leak pitfall of
        # @lru_cache on an instance method (lru_cache holds a strong ref
        # to ``self``, preventing GC of the instance).
        # Values are either a ``dict`` (positive hit) or the ``_CACHE_MISS``
        # sentinel (negative hit — skill not found in DB).  The sentinel
        # prevents repeated DB queries for nonexistent skills.
        self._metadata_cache: dict[tuple, dict | object] = {}

    def list_skills(self) -> list[Skill]:
        """Return a snapshot of all registered in-memory skills (thread-safe)."""
        return list(self._skills.values())

    # ── Registration (builtin / marketplace Python skills) ────────

    def register(
        self,
        skill: Skill,
        *,
        is_active: bool = True,
        source: str = SOURCE_BUILTIN,
        created_by: str | None = None,
        category: str = "general",
        subcategory: str = "default",
        triggers: list | None = None,
        dependencies: list | None = None,
        priority: int = 5,
        cost_estimate: str = "medium",
        git_commit_hash: str | None = None,
        status: str = "active",
    ) -> None:
        """Register a Python skill (builtin or marketplace)."""
        if status not in _VALID_STATUSES:
            raise ValueError(f"Invalid status: {status}")
        if source not in _VALID_SOURCES:
            raise ValueError(f"Invalid source: {source}")
        if status == "draft":
            is_active = False

        skill_id = f"{skill.name}@{skill.version}"
        code_hash = self._compute_code_hash(skill)
        se_profile = skill.side_effect_profile.model_dump() if skill.side_effect_profile else None

        self._upsert_skill_row(
            skill_id=skill_id,
            skill_name=skill.name,
            version=skill.version,
            description=skill.description,
            skill_definition=skill.requirements.model_dump(),
            code_hash=code_hash,
            git_commit_hash=git_commit_hash,
            is_active=is_active,
            status=status,
            source=source,
            created_by=created_by,
            category=category,
            subcategory=subcategory,
            triggers=triggers,
            dependencies=dependencies,
            priority=priority,
            cost_estimate=cost_estimate,
            side_effect_profile=se_profile,
        )

        # In-memory cache
        key = f"{skill.name}@{skill.version}"
        self._skills[key] = skill
        if is_active:
            self._skills[skill.name] = skill
        self._invalidate_cache()

        if self.gate_trigger and is_active and status == "active":
            self.gate_trigger.on_skill_change(
                skill_name=skill.name,
                version=skill.version,
                definition=skill.requirements.model_dump(),
            )

    def register_from_api(
        self,
        *,
        skill_id: str,
        skill_name: str,
        version: str,
        skill_code: str,
        description: str = "",
        metadata: dict | None = None,
        source: str = SOURCE_BUILTIN,
        created_by: str | None = None,
    ) -> dict[str, Any]:
        """Register a skill from an API request (no in-memory Skill object).

        This is the correct entry point for the REST API's register endpoint.
        It goes through the same DB upsert as register() — deactivation of old
        versions, code hash, gate trigger, source/status validation — but
        accepts raw fields instead of a Skill instance.
        """
        if source not in _VALID_SOURCES:
            raise ValueError(f"Invalid source: {source}")

        code_hash = hashlib.sha256(skill_code.encode()).hexdigest()
        skill_definition = metadata or {}

        self._upsert_skill_row(
            skill_id=skill_id,
            skill_name=skill_name,
            version=version,
            description=description,
            skill_definition=skill_definition,
            code_hash=code_hash,
            is_active=True,
            status="active",
            source=source,
            created_by=created_by,
            category=skill_definition.get("category", "general"),
            subcategory="default",
            triggers=[],
            dependencies=[],
            priority=5,
            cost_estimate="medium",
            side_effect_profile={"category": "read"},
        )

        # Re-read to get server-generated fields (created_at)
        with self._db() as db:
            row = db.query(SkillModel).filter(SkillModel.skill_id == skill_id).first()
            result = {
                "skill_id": row.skill_id,
                "skill_name": row.skill_name,
                "version": row.version,
                "description": row.description,
                "metadata": row.skill_definition or {},
                "created_at": row.created_at.isoformat() if row.created_at else None,
            }

        self._invalidate_cache()

        # Gate trigger — register_from_api always creates active skills,
        # but guard on status for forward-compatibility if status param is
        # added later.  Matches the guard in register().
        if self.gate_trigger:
            self.gate_trigger.on_skill_change(
                skill_name=skill_name,
                version=version,
                definition=skill_definition,
            )

        return result

    def _upsert_skill_row(
        self,
        *,
        skill_id: str,
        skill_name: str,
        version: str,
        description: str,
        skill_definition: dict,
        code_hash: str,
        is_active: bool,
        status: str,
        source: str,
        created_by: str | None,
        category: str,
        subcategory: str,
        triggers: list | None,
        dependencies: list | None,
        priority: int,
        cost_estimate: str,
        side_effect_profile: dict | None,
        git_commit_hash: str | None = None,
    ) -> None:
        """Shared DB upsert logic for register() and register_from_api().

        Deactivates old versions of the same skill (if ``is_active``),
        then inserts or updates the row.  Computes embedding inline when
        ``embed_fn`` is available.
        """
        # Compute embedding from name + description + triggers
        embedding_val = self._compute_embedding(skill_name, description, triggers)

        with self._db() as db:
            if is_active:
                db.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                ).update({"is_active": 0})

            existing = db.query(SkillModel).filter(SkillModel.skill_id == skill_id).first()
            if existing:
                existing.description = description
                existing.skill_definition = skill_definition
                existing.code_hash = code_hash
                existing.git_commit_hash = git_commit_hash
                existing.is_active = 1 if is_active else 0
                existing.status = status
                existing.source = source
                existing.created_by = created_by
                existing.category = category
                existing.subcategory = subcategory
                existing.triggers = triggers
                existing.dependencies = dependencies
                existing.priority = priority
                existing.cost_estimate = cost_estimate
                existing.side_effect_profile = side_effect_profile
                if embedding_val is not None:
                    existing.embedding = embedding_val
            else:
                db.add(SkillModel(
                    skill_id=skill_id,
                    skill_name=skill_name,
                    version=version,
                    description=description,
                    skill_definition=skill_definition,
                    code_hash=code_hash,
                    git_commit_hash=git_commit_hash,
                    is_active=1 if is_active else 0,
                    status=status,
                    source=source,
                    created_by=created_by,
                    category=category,
                    subcategory=subcategory,
                    triggers=triggers,
                    dependencies=dependencies,
                    priority=priority,
                    cost_estimate=cost_estimate,
                    side_effect_profile=side_effect_profile,
                    embedding=embedding_val,
                ))
            db.commit()

    def _compute_embedding(
        self, name: str, description: str | None, triggers: list | None,
    ) -> str | None:
        """Return vector literal for DB storage, or None if embedding unavailable."""
        if not self._embed_fn:
            return None
        parts = [name]
        if description:
            parts.append(description)
        if triggers:
            parts.append(" ".join(str(t) for t in triggers))
        try:
            vec = self._embed_fn(" | ".join(parts))
            return "[" + ",".join(str(v) for v in vec) + "]"
        except Exception as e:
            logger.warning("Failed to embed skill %s: %s", name, e)
            return None

    # ── User skill publish / unpublish ────────────────────────────

    def publish_user_skill(
        self,
        user_id: str,
        name: str,
        version: str,
        description: str,
        *,
        triggers: list[str] | None = None,
        dependencies: list[str] | None = None,
        manifest: dict | None = None,
        category: str = "user",
        priority: int = 5,
    ) -> dict[str, Any]:
        """Publish a user-created skill to the platform."""
        with self._db() as db:
            self._check_name_conflict(db, name, SOURCE_USER, user_id)

            skill_id = f"{name}@{version}"
            existing = db.query(SkillModel).filter(SkillModel.skill_id == skill_id).first()

            if existing:
                # _check_name_conflict already verified ownership at the
                # skill_name level, so if we reach here the existing row
                # belongs to user_id (or is a new version of their skill).
                existing.description = description
                existing.triggers = triggers
                existing.dependencies = dependencies
                existing.manifest = manifest
                existing.category = category
                existing.priority = priority
                existing.is_active = 1
                existing.status = "active"
                # Deactivate other versions
                db.query(SkillModel).filter(
                    SkillModel.skill_name == name,
                    SkillModel.skill_id != skill_id,
                    SkillModel.created_by == user_id,
                ).update({"is_active": 0})
            else:
                # Deactivate old versions by same user
                db.query(SkillModel).filter(
                    SkillModel.skill_name == name,
                    SkillModel.created_by == user_id,
                ).update({"is_active": 0})
                db.add(SkillModel(
                    skill_id=skill_id,
                    skill_name=name,
                    version=version,
                    description=description,
                    is_active=1,
                    status="active",
                    source=SOURCE_USER,
                    created_by=user_id,
                    is_public=0,
                    triggers=triggers,
                    dependencies=dependencies,
                    manifest=manifest,
                    category=category,
                    priority=priority,
                    cost_estimate="low",
                ))
            db.commit()

        self._invalidate_cache()
        return {"skill_id": skill_id, "skill_name": name, "version": version, "status": "active"}

    def unpublish_user_skill(self, user_id: str, name: str) -> str:
        """Unpublish a user skill. Returns 'deprecated' or 'deleted'."""
        from api.models import SkillInstallation

        with self._db() as db:
            rows = db.query(SkillModel).filter(
                SkillModel.skill_name == name,
                SkillModel.source == SOURCE_USER,
                SkillModel.created_by == user_id,
            ).all()
            if not rows:
                raise SkillNotFoundError(name)

            # Check if other users have installed it
            install_count = db.query(SkillInstallation).filter(
                SkillInstallation.skill_name == name,
                SkillInstallation.status == "installed",
                SkillInstallation.user_id != user_id,
            ).count()

            if install_count > 0:
                for row in rows:
                    row.status = "deprecated"
                    row.is_active = 0
                db.commit()
                self._invalidate_cache()
                return "deprecated"
            else:
                db.query(SkillModel).filter(
                    SkillModel.skill_name == name,
                    SkillModel.source == SOURCE_USER,
                    SkillModel.created_by == user_id,
                ).delete(synchronize_session="fetch")
                db.commit()
                self._evict(name)
                return "deleted"

    # ── Conflict detection ────────────────────────────────────────

    def _check_name_conflict(self, db: Session, name: str, source: str, user_id: str | None) -> None:
        """Check for name conflicts based on source hierarchy."""
        existing = db.query(SkillModel).filter(SkillModel.skill_name == name).first()
        if not existing:
            return

        if existing.source == SOURCE_BUILTIN:
            raise NameConflictError(name, "reserved by builtin skill")

        if source == SOURCE_USER:
            if existing.source == SOURCE_MARKETPLACE:
                raise NameConflictError(name, "already exists as marketplace skill")
            if existing.source == SOURCE_USER and existing.created_by != user_id:
                raise NameConflictError(name, "owned by another user")

        if source == SOURCE_MARKETPLACE and existing.source == SOURCE_MARKETPLACE:
            raise NameConflictError(name, "marketplace skill already exists")

    # ── Lifecycle transitions ─────────────────────────────────────

    def set_status(self, skill_name: str, version: str, status: str) -> bool:
        """Transition skill lifecycle status.

        This is the single code path for all status transitions.  Higher-level
        helpers (``activate``, ``deprecate``) delegate here so that transition
        validation and gate-trigger logic are never duplicated.
        """
        if status not in _VALID_STATUSES:
            raise ValueError(f"Invalid status: {status}")

        trigger_gate = False
        definition: dict = {}

        with self._db() as db:
            # Look up by (skill_name, version) — the skill_id primary key may
            # be an opaque string rather than the "name@version" convention.
            existing = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name,
                SkillModel.version == version,
            ).first()
            if not existing:
                return False

            old_status = existing.status or "active"
            if status not in _STATUS_TRANSITIONS.get(old_status, set()):
                raise ValueError(f"Invalid transition: {old_status} → {status}")

            existing.status = status
            if status == "active":
                db.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.skill_id != existing.skill_id,
                ).update({"is_active": 0})
                existing.is_active = 1
                if self.gate_trigger:
                    definition = existing.skill_definition or {}
                    trigger_gate = True
            elif status in ("deprecated", "archived"):
                existing.is_active = 0
            db.commit()

        self._invalidate_cache()
        if trigger_gate:
            self.gate_trigger.on_skill_change(skill_name, version, definition)
        return True

    def activate(self, skill_name: str, version: str | None = None) -> None:
        """Transition skill from draft/deprecated → active.

        If ``version`` is not given, activates the latest non-active version.
        Delegates to ``set_status`` so transition rules are enforced uniformly.
        """
        if version:
            self.set_status(skill_name, version, "active")
            return

        with self._db() as db:
            row = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name,
                SkillModel.status.in_(("draft", "deprecated")),
            ).order_by(SkillModel.created_at.desc()).first()
            if not row:
                raise ValueError(f"No draft/deprecated version of '{skill_name}' to activate")
            target_version = row.version

        self.set_status(skill_name, target_version, "active")

    # Backward-compatible alias — marketplace.py and tests call publish().
    publish = activate

    def deprecate(self, skill_name: str) -> None:
        """Transition the active version of a skill → deprecated.

        Delegates to ``set_status`` so transition rules are enforced uniformly.
        """
        with self._db() as db:
            row = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name, SkillModel.is_active == 1,
            ).first()
            if not row:
                raise ValueError(f"No active skill '{skill_name}' to deprecate")
            target_version = row.version

        self.set_status(skill_name, target_version, "deprecated")

    def rollback(self, skill_name: str) -> str:
        """Rollback to the most recent superseded version.

        Returns the activated version string.

        **Superseded vs deprecated**: when ``register()`` deactivates old
        versions it only sets ``is_active=0`` — the ``status`` stays
        ``"active"``.  These are "superseded" versions: they were never
        explicitly deprecated, just replaced by a newer registration.
        ``rollback()`` finds the most recent such version.

        If a version was explicitly deprecated (via ``deprecate()`` or
        ``set_status``), rollback skips it — the user made a deliberate
        decision to retire that version.

        Does NOT delegate to ``set_status()`` because rollback is an
        atomic two-row swap (current → deprecated, previous → active).
        The previous version already has ``status='active'`` (just
        ``is_active=0``), so ``set_status('active')`` would reject it
        as an ``active → active`` no-op.
        """
        with self._db() as db:
            current = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name, SkillModel.is_active == 1,
            ).first()
            if not current:
                raise ValueError(f"No active version of '{skill_name}' to rollback")

            previous = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name,
                SkillModel.is_active == 0,
                SkillModel.status == "active",
            ).order_by(SkillModel.created_at.desc(), SkillModel.skill_id.desc()).first()
            if not previous:
                raise ValueError(f"No previous version of '{skill_name}' to rollback to")

            current.is_active = 0
            current.status = "deprecated"
            previous.is_active = 1
            previous.status = "active"
            activated_version = previous.version
            old_version = current.version
            db.commit()

        self._skills.pop(skill_name, None)
        self._invalidate_cache()
        logger.info("Rolled back %s: %s → %s", skill_name, old_version, activated_version)
        return activated_version

    def uninstall(self, skill_name: str) -> int:
        """Remove all versions of a skill from catalog. Returns count removed."""
        with self._db() as db:
            count = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name,
            ).delete(synchronize_session="fetch")
            db.commit()
        self._evict(skill_name)
        logger.info("Uninstalled %s (%d versions)", skill_name, count)
        return count

    # ── Queries ───────────────────────────────────────────────────

    def get(self, skill_name: str, version: str | None = None) -> Skill:
        """Get in-memory Skill instance.

        Raises SkillNotFoundError if the skill was never registered via
        ``register()`` in this process.  API-only skills (registered via
        ``register_from_api``) have no in-memory Skill object.
        """
        key = f"{skill_name}@{version}" if version else skill_name
        skill = self._skills.get(key)
        if skill is None:
            raise SkillNotFoundError(skill_name, version)
        return skill

    def get_metadata(self, skill_name: str) -> dict | None:
        """Get skill metadata from DB (active version)."""
        return self._query_metadata(skill_name)

    def get_metadata_by_id(self, skill_id: str) -> dict | None:
        """Get skill metadata by exact skill_id (primary key).

        Cached — uses a ``("__by_id__", skill_id)`` cache key to avoid
        collisions with the ``(skill_name, timestamp, commit)`` keys used
        by ``_query_metadata``.
        """
        cache_key = ("__by_id__", skill_id)
        cached = self._metadata_cache.get(cache_key)
        if cached is _CACHE_MISS:
            return None
        if cached is not None:
            return cached

        with self._db() as db:
            row = db.query(SkillModel).filter(SkillModel.skill_id == skill_id).first()
            if not row:
                self._metadata_cache[cache_key] = _CACHE_MISS
                return None
            result = self._row_to_dict(row)

        self._metadata_cache[cache_key] = result
        return result

    def get_as_of(
        self,
        skill_name: str,
        as_of_timestamp: datetime | None = None,
        as_of_commit: str | None = None,
    ) -> dict | None:
        """Get skill metadata as of a specific timestamp or commit."""
        if as_of_commit:
            return self._query_metadata(skill_name, commit=as_of_commit)
        elif as_of_timestamp:
            return self._query_metadata(skill_name, timestamp=as_of_timestamp.isoformat())
        return self._query_metadata(skill_name)

    def _query_metadata(
        self, skill_name: str, timestamp: str | None = None, commit: str | None = None,
    ) -> dict | None:
        """Cached DB query for skill metadata.

        Uses a plain dict cache instead of ``@lru_cache`` to avoid the
        instance-method memory-leak problem (lru_cache holds a strong
        reference to ``self``, preventing GC).  Negative results (skill
        not found) are cached as ``_CACHE_MISS`` so repeated lookups of
        nonexistent skills don't hit the DB.
        """
        cache_key = (skill_name, timestamp, commit)
        cached = self._metadata_cache.get(cache_key)
        if cached is _CACHE_MISS:
            return None
        if cached is not None:
            return cached

        with self._db() as db:
            if commit:
                skill = db.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.git_commit_hash == commit,
                ).order_by(SkillModel.created_at.desc()).first()
            elif timestamp:
                dt = datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
                skill = db.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.created_at <= dt,
                ).order_by(SkillModel.created_at.desc()).first()
            else:
                skill = db.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.is_active == 1,
                ).first()

            if not skill:
                self._metadata_cache[cache_key] = _CACHE_MISS
                return None

            result = self._row_to_dict(skill)

        self._metadata_cache[cache_key] = result
        return result

    def list_by_source(self, source: str, limit: int = 100) -> list[dict[str, Any]]:
        """List skills filtered by source.
        
        Args:
            source: Skill source filter
            limit: Max results (default 100, prevents unbounded queries)
        """
        with self._db() as db:
            # Projection: skip embedding column (not returned by _row_to_dict anyway)
            rows = db.query(
                SkillModel.skill_id,
                SkillModel.skill_name,
                SkillModel.version,
                SkillModel.description,
                SkillModel.skill_definition,
                SkillModel.git_commit_hash,
                SkillModel.source,
                SkillModel.status,
                SkillModel.is_active,
                SkillModel.created_by,
                SkillModel.category,
                SkillModel.cost_estimate,
                SkillModel.triggers,
                SkillModel.dependencies,
                SkillModel.priority,
                SkillModel.created_at,
            ).filter(
                SkillModel.source == source, SkillModel.is_active == 1,
            ).order_by(SkillModel.created_at.desc()).limit(limit).all()
            # Convert Row tuples to dict manually (can't use _row_to_dict with tuples)
            return [
                {
                    "skill_id": r.skill_id,
                    "skill_name": r.skill_name,
                    "version": r.version,
                    "description": r.description,
                    "skill_definition": r.skill_definition,
                    "git_commit_hash": r.git_commit_hash,
                    "source": r.source,
                    "status": r.status,
                    "is_active": r.is_active,
                    "created_by": r.created_by,
                    "category": r.category,
                    "cost_estimate": r.cost_estimate,
                    "triggers": r.triggers,
                    "dependencies": r.dependencies,
                    "priority": r.priority,
                    "created_at": r.created_at.isoformat() if r.created_at else None,
                }
                for r in rows
            ]

    def list_by_owner(self, user_id: str, limit: int = 100) -> list[dict[str, Any]]:
        """List skills created by a specific user.
        
        Args:
            user_id: User ID filter
            limit: Max results (default 100, prevents unbounded queries)
        """
        with self._db() as db:
            # Projection: skip embedding column
            rows = db.query(
                SkillModel.skill_id,
                SkillModel.skill_name,
                SkillModel.version,
                SkillModel.description,
                SkillModel.skill_definition,
                SkillModel.git_commit_hash,
                SkillModel.source,
                SkillModel.status,
                SkillModel.is_active,
                SkillModel.created_by,
                SkillModel.category,
                SkillModel.cost_estimate,
                SkillModel.triggers,
                SkillModel.dependencies,
                SkillModel.priority,
                SkillModel.created_at,
            ).filter(
                SkillModel.created_by == user_id,
            ).order_by(SkillModel.created_at.desc()).limit(limit).all()
            return [
                {
                    "skill_id": r.skill_id,
                    "skill_name": r.skill_name,
                    "version": r.version,
                    "description": r.description,
                    "skill_definition": r.skill_definition,
                    "git_commit_hash": r.git_commit_hash,
                    "source": r.source,
                    "status": r.status,
                    "is_active": r.is_active,
                    "created_by": r.created_by,
                    "category": r.category,
                    "cost_estimate": r.cost_estimate,
                    "triggers": r.triggers,
                    "dependencies": r.dependencies,
                    "priority": r.priority,
                    "created_at": r.created_at.isoformat() if r.created_at else None,
                }
                for r in rows
            ]

    def list_active(self, limit: int = 100, offset: int = 0) -> dict[str, Any]:
        """List all active skills with pagination."""
        with self._db() as db:
            total = db.query(SkillModel).filter(SkillModel.is_active == 1).count()
            rows = db.query(SkillModel).filter(
                SkillModel.is_active == 1,
            ).order_by(SkillModel.created_at.desc()).offset(offset).limit(limit).all()
            return {
                "skills": [self._row_to_dict(r) for r in rows],
                "total": total,
                "limit": limit,
                "offset": offset,
            }

    def get_skill_info(self, name: str, user_id: str | None = None) -> dict[str, Any] | None:
        """Get enriched skill info including install count."""
        from api.models import SkillInstallation

        with self._db() as db:
            skill = db.query(SkillModel).filter(
                SkillModel.skill_name == name, SkillModel.is_active == 1,
            ).first()
            if not skill:
                return None

            install_count = db.query(SkillInstallation).filter(
                SkillInstallation.skill_name == name,
                SkillInstallation.status == "installed",
            ).count()

            info = self._row_to_dict(skill)
            info["install_count"] = install_count
            return info

    def get_visible_skills(
        self, user_id: str, *, per_group: int = 50,
    ) -> dict[str, Any]:
        """Get skills visible to a user, grouped by source.

        Each group returns at most *per_group* items plus a total count,
        so the caller knows whether to fetch more via the dedicated
        ``GET /skills?source=...`` list endpoint.
        """
        from api.models import SkillInstallation

        with self._db() as db:
            # Builtin + marketplace (active)
            platform_q = db.query(SkillModel).filter(
                SkillModel.is_active == 1,
                SkillModel.source.in_([SOURCE_BUILTIN, SOURCE_MARKETPLACE]),
            )
            platform_total = platform_q.count()
            platform_skills = (
                platform_q.order_by(SkillModel.created_at.desc())
                .limit(per_group).all()
            )

            # User's own published skills
            user_q = db.query(SkillModel).filter(
                SkillModel.created_by == user_id,
                SkillModel.source == SOURCE_USER,
            )
            user_total = user_q.count()
            user_skills = (
                user_q.order_by(SkillModel.created_at.desc())
                .limit(per_group).all()
            )

            # Installed skill names — scoped to current page across all groups
            page_names = (
                [s.skill_name for s in platform_skills]
                + [s.skill_name for s in user_skills]
            )
            if page_names:
                installed_names = {
                    r.skill_name for r in db.query(
                        SkillInstallation.skill_name,
                    ).filter(
                        SkillInstallation.user_id == user_id,
                        SkillInstallation.status == "installed",
                        SkillInstallation.skill_name.in_(page_names),
                    ).all()
                }
            else:
                installed_names = set()

            result: dict[str, Any] = {
                SOURCE_BUILTIN: [],
                SOURCE_MARKETPLACE: [],
                SOURCE_USER: [],
                "platform_total": platform_total,
                "user_total": user_total,
            }
            for s in platform_skills:
                d = self._row_to_dict(s)
                d["installed"] = s.skill_name in installed_names
                result[s.source or SOURCE_BUILTIN].append(d)
            for s in user_skills:
                d = self._row_to_dict(s)
                d["installed"] = s.skill_name in installed_names
                result[SOURCE_USER].append(d)

        return result

    def list_versions(self, skill_name: str) -> list[dict[str, Any]]:
        """List all versions of a skill."""
        with self._db() as db:
            rows = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name,
            ).order_by(SkillModel.created_at.desc()).all()
            return [
                {"version": r.version, "status": r.status, "is_active": r.is_active,
                 "created_at": r.created_at.isoformat() if r.created_at else None}
                for r in rows
            ]

    def list_available(self, repo_id: str) -> list[Skill]:
        """List in-memory skills compatible with a repo's type and access scope.

        Queries the repo's type/access from DB, then filters the in-memory
        skill cache by SkillRequirement compatibility.
        """
        from api.models import Repo as RepoModel

        with self._db() as db:
            repo = db.query(RepoModel).filter(RepoModel.repo_id == repo_id).first()
            if not repo:
                return []
            repo_type = repo.repo_type
            repo_access = repo.access_scope

        available = []
        for key, skill in self._skills.items():
            if "@" in key:  # skip versioned keys, only check active aliases
                continue
            req = skill.requirements
            type_ok = repo_type in [rt.value for rt in req.repo_types]
            access_ok = _ACCESS_ORDER.get(repo_access, 0) >= _ACCESS_ORDER.get(req.min_access.value, 0)
            if type_ok and access_ok:
                available.append(skill)
        return available

    # ── Internal helpers ──────────────────────────────────────────

    def _invalidate_cache(self) -> None:
        """Clear the metadata cache after any mutation."""
        self._metadata_cache.clear()

    def _evict(self, skill_name: str) -> None:
        """Remove skill from in-memory cache."""
        keys = [k for k in self._skills if k == skill_name or k.startswith(f"{skill_name}@")]
        for k in keys:
            del self._skills[k]
        self._invalidate_cache()

    @staticmethod
    def _row_to_dict(row: SkillModel) -> dict[str, Any]:
        """Convert ORM row to a complete metadata dict.

        This is the single canonical dict shape for skill metadata.
        All query methods (``_query_metadata``, ``get_metadata_by_id``,
        ``list_*``, ``get_skill_info``, etc.) return dicts with this shape
        so callers never need to guess which fields are present.
        """
        return {
            "skill_id": row.skill_id,
            "skill_name": row.skill_name,
            "version": row.version,
            "description": row.description,
            "skill_definition": row.skill_definition,
            "git_commit_hash": row.git_commit_hash,
            "source": row.source,
            "status": row.status,
            "is_active": row.is_active,
            "created_by": row.created_by,
            "category": getattr(row, "category", None),
            "cost_estimate": getattr(row, "cost_estimate", None),
            "triggers": getattr(row, "triggers", None),
            "dependencies": getattr(row, "dependencies", None),
            "priority": getattr(row, "priority", None),
            "created_at": row.created_at.isoformat() if row.created_at else None,
        }

    @staticmethod
    def _compute_code_hash(skill: Skill) -> str:
        try:
            code = inspect.getsource(skill.__class__)
        except (OSError, TypeError):
            cls = skill.__class__
            code = f"{cls.__module__}.{cls.__qualname__}:{getattr(skill, 'version', '')}"
        return hashlib.sha256(code.encode()).hexdigest()
