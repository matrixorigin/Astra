"""Skill registry for managing skill lifecycle and versioning."""

import hashlib
import inspect
import json
from datetime import datetime
from functools import lru_cache

from sqlalchemy.orm import Session

from api.models import SkillRegistry as SkillModel
from core.exceptions import DatabaseError, SkillNotFoundError
from core.logging_config import get_logger

from .base import Skill

logger = get_logger(__name__)


class SkillRegistry:
    """Manage skill metadata and lifecycle with versioning"""

    def __init__(self, session: Session, gate_trigger=None):
        if not isinstance(session, Session):
            raise TypeError("session must be a SQLAlchemy Session")
        
        self.session = session
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
        """
        logger.info(f"Registering skill: {skill.name}@{skill.version}")

        try:
            
            # 1. Deactivate old versions if this is active
            if is_active:
                self.session.query(SkillModel).filter(
                    SkillModel.skill_name == skill.name
                ).update({"is_active": 0})

            # 2. Compute code hash
            code_hash = self._compute_code_hash(skill)

            # 3. Check if skill already exists
            skill_id = f"{skill.name}@{skill.version}"
            existing = self.session.query(SkillModel).filter(SkillModel.skill_id == skill_id).first()
            
            if existing:
                # Update existing
                existing.description = skill.description
                existing.skill_definition = skill.requirements.model_dump()
                existing.code_hash = code_hash
                existing.git_commit_hash = git_commit_hash
                existing.is_active = 1 if is_active else 0
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
                    category=category,
                    subcategory=subcategory,
                    triggers=triggers,
                    dependencies=dependencies,
                    priority=priority,
                    cost_estimate=cost_estimate,
                )
                self.session.add(skill_model)
            
            self.session.commit()

            # 4. Store in memory
            key = f"{skill.name}@{skill.version}"
            self._skills[key] = skill
            if is_active:
                self._skills[skill.name] = skill  # Shortcut to active version

            # 5. Clear LRU cache on new registration
            self._get_cached.cache_clear()

            # 6. Auto-trigger regression gate (async, non-blocking)
            if self.gate_trigger and is_active:
                self.gate_trigger.on_skill_change(
                    skill_name=skill.name,
                    version=skill.version,
                    definition=skill.requirements.model_dump(),
                )

            logger.info(
                f"Successfully registered skill: {skill.name}@{skill.version} (category={category}, priority={priority})"
            )

        except Exception as e:
            logger.error(f"Failed to register skill {skill.name}@{skill.version}: {e}")
            raise DatabaseError(f"Failed to register skill: {e}") from e

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

        Args:
            skill_name: Skill name
            timestamp: ISO format timestamp for AS OF query
            commit: Git commit hash for AS OF query

        Returns:
            Skill metadata dict or None
        """
        try:
            if commit:
                # Query by commit hash
                skill = self.session.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.git_commit_hash == commit
                ).order_by(SkillModel.created_at.desc()).first()
            elif timestamp:
                # Query by timestamp
                try:
                    from datetime import datetime
                    dt = datetime.fromisoformat(timestamp.replace('Z', '+00:00'))
                except Exception:
                    dt = datetime.fromisoformat(timestamp)
                
                skill = self.session.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.created_at <= dt
                ).order_by(SkillModel.created_at.desc()).first()
            else:
                # Current active version
                skill = self.session.query(SkillModel).filter(
                    SkillModel.skill_name == skill_name,
                    SkillModel.is_active == 1
                ).first()

            if skill:
                result = {
                    "skill_id": skill.skill_id,
                    "skill_name": skill.skill_name,
                    "version": skill.version,
                    "skill_definition": skill.skill_definition,
                    "git_commit_hash": skill.git_commit_hash,
                    "is_active": skill.is_active,
                    "created_at": skill.created_at,
                }
                # Add optional metadata fields
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
            return None

        except Exception as e:
            logger.error(f"Failed to query skill {skill_name}: {e}")
            return None

    def list_available(self, repo_id: int) -> list[Skill]:
        """List skills available for a repo"""
        logger.debug(f"Listing available skills for repo {repo_id}")

        # Query repo type and access scope
        from api.models import Repo
        repo = self.session.query(Repo).filter(Repo.repo_id == str(repo_id)).first()

        if not repo:
            logger.warning(f"Repo not found: {repo_id}")
            return []

        # Filter skills by requirements
        available = []
        for key, skill in self._skills.items():
            if "@" in key:  # Skip versioned keys, only check active
                continue

            if repo.repo_type in [
                rt.value for rt in skill.requirements.repo_types
            ] and self._has_access(repo.access_scope, skill.requirements.min_access.value):
                available.append(skill)

        logger.debug(f"Found {len(available)} available skills for repo {repo_id}")
        return available

    def _has_access(self, current: str, required: str) -> bool:
        """Check if current access meets requirement"""
        levels = {"read": 1, "write": 2, "admin": 3}
        return levels.get(current, 0) >= levels.get(required, 0)

    def _compute_code_hash(self, skill: Skill) -> str:
        """Compute SHA256 hash of skill code for verification"""
        try:
            code = inspect.getsource(skill.__class__)
            return hashlib.sha256(code.encode()).hexdigest()
        except Exception:
            return "unknown"
