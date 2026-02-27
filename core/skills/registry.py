"""Skill registry for managing skill lifecycle and versioning."""

from __future__ import annotations

import hashlib
import inspect
from collections.abc import Callable
from datetime import datetime
from functools import lru_cache
from typing import TYPE_CHECKING

from sqlalchemy.orm import Session

from api.models import SkillRegistry as SkillModel
from core.exceptions import DatabaseError, SkillNotFoundError
from core.logging_config import get_logger

from .base import AccessScope, Skill

if TYPE_CHECKING:
    from core.evaluation.gate_trigger import GateTrigger

logger = get_logger(__name__)


class SkillRegistry:
    """Manage skill metadata and lifecycle with versioning"""

    def __init__(
        self,
        db_factory: Callable[[], Session],
        gate_trigger: GateTrigger | None = None,
    ):
        self._db_factory = db_factory
        self.gate_trigger = gate_trigger
        self._skills: dict[str, Skill] = {}  # skill_name@version -> Skill
        self._cache_size = 100  # LRU cache size

    def register(
        self,
        skill: Skill,
        is_active: bool = True,
        category: str = "general",
        subcategory: str = "default",
        triggers: list | None = None,
        dependencies: list | None = None,
        priority: int = 5,
        cost_estimate: str = "medium",
        git_commit_hash: str | None = None,
        status: str = "active",
    ) -> None:
        """Register a skill version with metadata.

        Args:
            skill: Skill instance
            is_active: Whether this version is active
            category: Skill category (github/code/docs)
            subcategory: Skill subcategory
            triggers: Trigger keywords for selection
            dependencies: Dependent skill names
            priority: Priority (1-10)
            cost_estimate: Cost estimate (low/medium/high)
            git_commit_hash: Git commit hash for precise replay
            status: Lifecycle status (draft/active/deprecated/archived)
        """
        if status not in ("draft", "active", "deprecated", "archived"):
            raise ValueError(f"Invalid skill status: {status}. Must be draft/active/deprecated/archived")

        # Draft skills are never active
        if status == "draft":
            is_active = False
        logger.info(f"Registering skill: {skill.name}@{skill.version}")

        db = self._db_factory()
        try:
            # 1. Deactivate old versions if this is active
            if is_active:
                db.query(SkillModel).filter(
                    SkillModel.skill_name == skill.name
                ).update({"is_active": 0})

            # 2. Compute code hash
            code_hash = self._compute_code_hash(skill)

            # 3. Check if skill already exists
            skill_id = f"{skill.name}@{skill.version}"
            existing = db.query(SkillModel).filter(SkillModel.skill_id == skill_id).first()

            if existing:
                # Update existing
                existing.description = skill.description
                existing.skill_definition = skill.requirements.model_dump()
                existing.code_hash = code_hash
                existing.git_commit_hash = git_commit_hash
                existing.is_active = 1 if is_active else 0
                existing.status = status
                existing.category = category
                existing.subcategory = subcategory
                existing.triggers = triggers
                existing.dependencies = dependencies
                existing.priority = priority
                existing.cost_estimate = cost_estimate
            else:
                # Insert new
                skill_model = SkillModel(
                    skill_id=skill_id,
                    skill_name=skill.name,
                    version=skill.version,
                    description=skill.description,
                    skill_definition=skill.requirements.model_dump(),
                    code_hash=code_hash,
                    git_commit_hash=git_commit_hash,
                    is_active=1 if is_active else 0,
                    status=status,
                    category=category,
                    subcategory=subcategory,
                    triggers=triggers,
                    dependencies=dependencies,
                    priority=priority,
                    cost_estimate=cost_estimate,
                )
                db.add(skill_model)

            db.commit()
        except Exception as e:
            db.rollback()
            logger.error(f"Failed to register skill {skill.name}@{skill.version}: {e}")
            raise DatabaseError(f"Failed to register skill: {e}") from e
        finally:
            db.close()

        # Post-commit side-effects (no DB needed)
        key = f"{skill.name}@{skill.version}"
        self._skills[key] = skill
        if is_active:
            self._skills[skill.name] = skill  # Shortcut to active version

        self._get_cached.cache_clear()

        # Auto-trigger regression gate (async, non-blocking)
        # Only for active skills — draft skills skip gate
        if self.gate_trigger and is_active and status == "active":
            self.gate_trigger.on_skill_change(
                skill_name=skill.name,
                version=skill.version,
                definition=skill.requirements.model_dump(),
            )

        logger.info(
            f"Successfully registered skill: {skill.name}@{skill.version} "
            f"(status={status}, category={category}, priority={priority})"
        )

    def set_status(self, skill_name: str, version: str, status: str) -> bool:
        """Transition skill lifecycle status.

        Valid transitions: draft→active, active→deprecated, deprecated→archived.
        Activating a draft skill triggers regression gate.
        """
        if status not in ("draft", "active", "deprecated", "archived"):
            raise ValueError(f"Invalid status: {status}")

        skill_id = f"{skill_name}@{version}"
        trigger_gate = False
        definition: dict = {}
        old_status = ""
        db = self._db_factory()
        try:
            existing = db.query(SkillModel).filter(
                SkillModel.skill_id == skill_id
            ).first()
            if not existing:
                return False

            old_status = existing.status or "active"
            valid_transitions = {
                "draft": {"active"},
                "active": {"deprecated"},
                "deprecated": {"archived", "active"},
                "archived": set(),
            }
            if status not in valid_transitions.get(old_status, set()):
                raise ValueError(f"Invalid transition: {old_status} → {status}")

            existing.status = status
            if status == "active":
                # Deactivate other versions of same skill
                db.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.skill_id != skill_id,
                ).update({"is_active": 0})
                existing.is_active = 1
                if self.gate_trigger:
                    # Extract ORM data before closing session
                    definition = existing.skill_definition or {}
                    trigger_gate = True
            elif status in ("deprecated", "archived"):
                existing.is_active = 0

            db.commit()
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()

        self._get_cached.cache_clear()
        logger.info(f"Skill {skill_id} status: {old_status} → {status}")

        if trigger_gate:
            self.gate_trigger.on_skill_change(
                skill_name=skill_name,
                version=version,
                definition=definition,
            )
        return True

    def get(self, skill_name: str, version: str | None = None) -> Skill | None:
        """Get skill by name and optional version

        Raises:
            SkillNotFoundError: If skill not found
        """
        if version:
            skill = self._skills.get(f"{skill_name}@{version}")
        else:
            skill = self._skills.get(skill_name)  # Active version

        if skill is None:
            logger.warning(f"Skill not found: {skill_name}@{version or 'active'}")
            raise SkillNotFoundError(skill_name, version)

        return skill

    def get_as_of(
        self,
        skill_name: str,
        as_of_timestamp: datetime | None = None,
        as_of_commit: str | None = None,
    ) -> dict | None:
        """Get skill metadata as of a specific timestamp or commit.

        Args:
            skill_name: Skill name
            as_of_timestamp: Query historical state at this timestamp
            as_of_commit: Query by git commit hash

        Returns:
            Skill metadata dict or None if not found
        """
        if as_of_commit:
            return self._get_cached(skill_name, commit=as_of_commit)
        elif as_of_timestamp:
            return self._get_cached(skill_name, timestamp=as_of_timestamp.isoformat())
        else:
            # Current version
            return self._get_cached(skill_name)

    @lru_cache(maxsize=100)
    def _get_cached(
        self, skill_name: str, timestamp: str | None = None, commit: str | None = None
    ) -> dict | None:
        """LRU cached query for skill metadata.

        Exceptions are intentionally NOT caught here — lru_cache only caches
        return values, not exceptions.  If we caught and returned None on a
        transient DB error, that None would be cached permanently, breaking
        all subsequent lookups until process restart.

        Args:
            skill_name: Skill name
            timestamp: ISO format timestamp for AS OF query
            commit: Git commit hash for AS OF query

        Returns:
            Skill metadata dict or None
        """
        db = self._db_factory()
        try:
            if commit:
                skill = db.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.git_commit_hash == commit,
                ).order_by(SkillModel.created_at.desc()).first()
            elif timestamp:
                try:
                    from datetime import datetime
                    dt = datetime.fromisoformat(timestamp.replace('Z', '+00:00'))
                except Exception:
                    dt = datetime.fromisoformat(timestamp)

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
                return None

            # Materialise all ORM attributes into a plain dict so the
            # result is safe to cache and use after the session is closed.
            result: dict = {
                "skill_id": skill.skill_id,
                "skill_name": skill.skill_name,
                "version": skill.version,
                "skill_definition": skill.skill_definition,
                "git_commit_hash": skill.git_commit_hash,
                "is_active": skill.is_active,
                "created_at": skill.created_at.isoformat() if skill.created_at else None,
            }
            if skill.cost_estimate:
                result["cost_estimate"] = skill.cost_estimate
            if skill.triggers:
                result["triggers"] = skill.triggers
            if skill.dependencies:
                result["dependencies"] = skill.dependencies
            if skill.category:
                result["category"] = skill.category
            if skill.priority is not None:
                result["priority"] = skill.priority
            return result
        finally:
            db.close()

    def list_available(self, repo_id: int) -> list[Skill]:
        """List skills available for a repo"""
        logger.debug(f"Listing available skills for repo {repo_id}")

        # Query repo type and access scope
        from api.models import Repo
        db = self._db_factory()
        try:
            repo = db.query(Repo).filter(Repo.repo_id == str(repo_id)).first()
            if not repo:
                logger.warning(f"Repo not found: {repo_id}")
                return []
            repo_type = repo.repo_type
            access_scope = repo.access_scope
        finally:
            db.close()

        # Filter skills by requirements
        available = []
        for key, skill in self._skills.items():
            if "@" in key:  # Skip versioned keys, only check active
                continue

            if repo_type in [
                rt.value for rt in skill.requirements.repo_types
            ] and self._has_access(access_scope, skill.requirements.min_access):
                available.append(skill)

        logger.debug(f"Found {len(available)} available skills for repo {repo_id}")
        return available

    def _has_access(self, current: str | AccessScope, required: str | AccessScope) -> bool:
        """Check if current access meets requirement"""
        levels = {"read": 1, "write": 2, "admin": 3}
        c = current.value if isinstance(current, AccessScope) else current
        r = required.value if isinstance(required, AccessScope) else required
        return levels.get(c, 0) >= levels.get(r, 0)

    def _compute_code_hash(self, skill: Skill) -> str:
        """Compute SHA256 hash of skill code for verification"""
        try:
            code = inspect.getsource(skill.__class__)
        except (OSError, TypeError):
            # Fallback for dynamic classes: hash qualified name + version
            cls = skill.__class__
            code = f"{cls.__module__}.{cls.__qualname__}:{getattr(skill, 'version', '')}"
        return hashlib.sha256(code.encode()).hexdigest()

    # ── Lifecycle transitions ──────────────────────────────────────

    def publish(self, skill_name: str) -> None:
        """Transition skill from draft → active. Triggers gate if configured."""
        from api.models import SkillRegistry as SkillModel
        db = self._db_factory()
        try:
            row = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name,
            ).first()
            if not row:
                raise ValueError(f"Skill '{skill_name}' not found")
            if row.status not in ("draft", "deprecated"):
                raise ValueError(f"Cannot publish skill in '{row.status}' state")
            row.status = "active"
            row.is_active = 1
            version = row.version
            manifest = row.manifest or {}
            db.commit()
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()
        self._get_cached.cache_clear()
        if self.gate_trigger:
            try:
                self.gate_trigger.on_skill_change(skill_name, version, manifest)
            except Exception as e:
                logger.warning("Gate trigger on publish failed: %s", e)

    def deprecate(self, skill_name: str) -> None:
        """Transition skill from active → deprecated."""
        from api.models import SkillRegistry as SkillModel
        db = self._db_factory()
        try:
            row = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name, SkillModel.status == "active",
            ).first()
            if not row:
                raise ValueError(f"No active skill '{skill_name}' to deprecate")
            row.status = "deprecated"
            row.is_active = 0
            db.commit()
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()
        self._get_cached.cache_clear()

    def rollback(self, skill_name: str) -> str:
        """Rollback to the previous version of a skill.

        Deactivates the current active version and reactivates the most recent
        non-active version. Returns the version that was activated.
        """
        db = self._db_factory()
        try:
            current = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name, SkillModel.is_active == 1,
            ).first()
            if not current:
                raise ValueError(f"No active version of '{skill_name}' to rollback")

            # Find the most recent inactive version that hasn't been deprecated.
            # "deprecated" means explicitly rolled back — not eligible for re-activation.
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
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()

        self._skills.pop(skill_name, None)
        self._get_cached.cache_clear()
        logger.info("Rolled back %s: %s → %s", skill_name, old_version, activated_version)
        return activated_version

    def uninstall(self, skill_name: str) -> int:
        """Remove all versions of a skill. Returns count of versions removed."""
        db = self._db_factory()
        try:
            count = db.query(SkillModel).filter(
                SkillModel.skill_name == skill_name,
            ).delete(synchronize_session="fetch")
            db.commit()
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()

        # Clean in-memory cache
        keys_to_remove = [k for k in self._skills if k == skill_name or k.startswith(f"{skill_name}@")]
        for k in keys_to_remove:
            del self._skills[k]
        self._get_cached.cache_clear()
        logger.info("Uninstalled %s (%d versions)", skill_name, count)
        return count
