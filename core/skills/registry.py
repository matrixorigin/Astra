"""Skill registry for managing skill lifecycle and versioning."""

import json
import hashlib
import inspect
from typing import Optional
from sdk import Database
from .base import Skill, AccessScope


class SkillRegistry:
    """Manage skill metadata and lifecycle with versioning"""

    def __init__(self, db: Database):
        self.db = db
        self._skills: dict[str, Skill] = {}  # skill_name@version -> Skill

    def register(self, skill: Skill, is_active: bool = True) -> None:
        """Register a skill version"""

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

        # 3. Insert new version
        self.db.execute(
            """
            INSERT INTO skills_registry 
            (skill_id, skill_name, version, description, requirements, 
             code_hash, is_active, status)
            VALUES (%s, %s, %s, %s, %s, %s, %s, 'active')
            ON DUPLICATE KEY UPDATE
                description = VALUES(description),
                requirements = VALUES(requirements),
                code_hash = VALUES(code_hash),
                is_active = VALUES(is_active),
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
            ),
        )

        # 4. Store in memory
        key = f"{skill.name}@{skill.version}"
        self._skills[key] = skill
        if is_active:
            self._skills[skill.name] = skill  # Shortcut to active version

    def get(self, skill_name: str, version: str = None) -> Optional[Skill]:
        """Get skill by name and optional version"""
        if version:
            return self._skills.get(f"{skill_name}@{version}")
        else:
            return self._skills.get(skill_name)  # Active version

    def list_available(self, repo_id: int) -> list[Skill]:
        """List skills available for a repo"""
        # Query repo type and access scope
        repo = self.db.fetchone(
            """
            SELECT repo_type, access_scope 
            FROM repos WHERE repo_id = %s
        """,
            (repo_id,),
        )

        if not repo:
            return []

        # Filter skills by requirements
        available = []
        for key, skill in self._skills.items():
            if "@" in key:  # Skip versioned keys, only check active
                continue

            if repo["repo_type"] in [
                rt.value for rt in skill.requirements.repo_types
            ] and self._has_access(
                repo["access_scope"], skill.requirements.min_access.value
            ):
                available.append(skill)

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
