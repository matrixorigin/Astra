"""Skill registry for managing skill lifecycle and versioning."""

import hashlib
import inspect
import json

from core.exceptions import DatabaseError, SkillNotFoundError
from core.logging_config import get_logger
from sdk import Database

from .base import Skill

logger = get_logger(__name__)


class SkillRegistry:
    """Manage skill metadata and lifecycle with versioning"""

    def __init__(self, db: Database):
        self.db = db
        self._skills: dict[str, Skill] = {}  # skill_name@version -> Skill

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
        """
        logger.info(f"Registering skill: {skill.name}@{skill.version}")

        try:
            # 1. Deactivate old versions if this is active
            if is_active:
                self.db.execute(
                    """
                    UPDATE skills_registry
                    SET is_active = 0
                    WHERE skill_name = %s
                """,
                    (skill.name,),
                )

            # 2. Compute code hash
            code_hash = self._compute_code_hash(skill)

            # 3. Insert new version with metadata
            self.db.execute(
                """
                INSERT INTO skills_registry
                (skill_id, skill_name, version, description, requirements,
                 code_hash, is_active, status, category, subcategory,
                 triggers, dependencies, priority, cost_estimate, side_effect_category)
                VALUES (%s, %s, %s, %s, %s, %s, %s, 'active', %s, %s, %s, %s, %s, %s, %s)
                ON DUPLICATE KEY UPDATE
                    description = VALUES(description),
                    requirements = VALUES(requirements),
                    code_hash = VALUES(code_hash),
                    is_active = VALUES(is_active),
                    category = VALUES(category),
                    subcategory = VALUES(subcategory),
                    triggers = VALUES(triggers),
                    dependencies = VALUES(dependencies),
                    priority = VALUES(priority),
                    cost_estimate = VALUES(cost_estimate),
                    side_effect_category = VALUES(side_effect_category),
                    updated_at = CURRENT_TIMESTAMP
            """,
                (
                    f"{skill.name}@{skill.version}",
                    skill.name,
                    skill.version,
                    skill.description,
                    json.dumps(skill.requirements.model_dump()),
                    code_hash,
                    1 if is_active else 0,
                    category,
                    subcategory,
                    json.dumps(triggers or []),
                    json.dumps(dependencies or []),
                    priority,
                    cost_estimate,
                    skill.side_effect_profile.category.value,
                ),
            )

            # 4. Store in memory
            key = f"{skill.name}@{skill.version}"
            self._skills[key] = skill
            if is_active:
                self._skills[skill.name] = skill  # Shortcut to active version

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

    def list_available(self, repo_id: int) -> list[Skill]:
        """List skills available for a repo"""
        logger.debug(f"Listing available skills for repo {repo_id}")

        # Query repo type and access scope
        repo = self.db.fetchone(
            """
            SELECT repo_type, access_scope
            FROM repos WHERE repo_id = %s
        """,
            (repo_id,),
        )

        if not repo:
            logger.warning(f"Repo not found: {repo_id}")
            return []

        # Filter skills by requirements
        available = []
        for key, skill in self._skills.items():
            if "@" in key:  # Skip versioned keys, only check active
                continue

            if repo["repo_type"] in [
                rt.value for rt in skill.requirements.repo_types
            ] and self._has_access(repo["access_scope"], skill.requirements.min_access.value):
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
