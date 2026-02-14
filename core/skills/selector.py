"""Skill selection and orchestration."""

from dataclasses import dataclass
from typing import Any

from sqlalchemy.orm import Session

from api.database import SessionLocal, get_db_session
from api.models import SkillRegistry as SkillModel
from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class SkillMetadata:
    """Extended skill metadata."""

    name: str
    version: str
    description: str
    category: str
    subcategory: str
    triggers: list[str]
    dependencies: list[str]
    priority: int
    cost_estimate: str


class SkillSelector:
    """Rule-based skill selector with keyword matching."""

    def __init__(self, session: Session | None = None):
        self._session = session
        self._owns_session = session is None
        self._load_skills()

    @property
    def session(self) -> Session:
        return self._get_session()

    def _get_session(self) -> Session:
        if self._session is None:
            self._session = SessionLocal()
            self._owns_session = True
        return self._session

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()

    def close(self):
        """Close the session if owned"""
        if self._owns_session and self._session:
            self._session.close()
            self._session = None

    def __del__(self):
        try:
            self.close()
        except Exception:
            pass

    def _load_skills(self):
        """Load skills with metadata from database."""
        self.skills = {}
        
        session = self._get_session()
        skills_data = session.query(SkillModel).filter(SkillModel.is_active == 1).all()

        for skill in skills_data:
            metadata = SkillMetadata(
                name=skill.skill_name,
                version=skill.version,
                description="",
                category="general",
                subcategory="default",
                triggers=[],
                dependencies=[],
                priority=5,
                cost_estimate="medium",
            )
            self.skills[metadata.name] = metadata

        logger.info(f"Loaded {len(self.skills)} skills from database")

    def select_skills(self, query: str, max_skills: int = 3) -> list[SkillMetadata]:
        """Select relevant skills based on query.

        Args:
            query: User query
            max_skills: Maximum number of skills to return

        Returns:
            List of selected skills
        """
        query_lower = query.lower()

        # 1. Keyword matching
        candidates = []
        for skill in self.skills.values():
            score = self._calculate_match_score(query_lower, skill)
            if score > 0:
                candidates.append((skill, score))

        if not candidates:
            logger.info("No skills matched query")
            return []

        # 2. Sort by score (priority * match_score)
        candidates.sort(key=lambda x: x[1], reverse=True)

        # 3. Select top skills
        selected = [skill for skill, score in candidates[:max_skills]]

        # 4. Resolve dependencies
        selected = self._resolve_dependencies(selected)

        logger.info(f"Selected skills: {[s.name for s in selected]}")

        return selected

    def _calculate_match_score(self, query: str, skill: SkillMetadata) -> float:
        """Calculate match score for skill."""
        score = 0.0

        # Check each trigger
        for trigger in skill.triggers:
            if trigger in query:
                # Exact match
                score += 1.0
            elif any(word in query for word in trigger.split()):
                # Partial match
                score += 0.5

        # Boost by priority
        score *= skill.priority / 10.0

        return score

    def _resolve_dependencies(self, skills: list[SkillMetadata]) -> list[SkillMetadata]:
        """Resolve skill dependencies."""
        result = list(skills)
        added = {s.name for s in skills}

        for skill in skills:
            for dep_name in skill.dependencies:
                if dep_name not in added and dep_name in self.skills:
                    result.insert(0, self.skills[dep_name])
                    added.add(dep_name)
                    logger.debug(f"Added dependency: {dep_name} for {skill.name}")

        return result

    def get_skill_by_name(self, name: str) -> SkillMetadata | None:
        """Get skill metadata by name."""
        skill = self.skills.get(name)
        return skill if isinstance(skill, SkillMetadata) else None

    def list_skills_by_category(self, category: str) -> list[SkillMetadata]:
        """List skills in a category."""
        return [s for s in self.skills.values() if s.category == category]


class SkillOrchestrator:
    """Orchestrate skill selection and execution."""

    def __init__(self, session: Session | None = None):
        self._session = session
        self.selector = SkillSelector(session)

    def plan_execution(self, query: str, context: dict[str, Any] | None = None) -> dict[str, Any]:
        """Plan skill execution for query.

        Args:
            query: User query
            context: Optional context information

        Returns:
            Execution plan with selected skills
        """
        # Select skills
        selected_skills = self.selector.select_skills(query)

        if not selected_skills:
            return {"skills": [], "execution_order": [], "estimated_cost": "none"}

        # Build execution plan
        plan = {
            "skills": [
                {
                    "name": s.name,
                    "description": s.description,
                    "category": s.category,
                    "priority": s.priority,
                }
                for s in selected_skills
            ],
            "execution_order": [s.name for s in selected_skills],
            "estimated_cost": self._estimate_total_cost(selected_skills),
            "dependencies_resolved": True,
        }

        logger.info(f"Execution plan: {plan['execution_order']}")

        return plan

    def _estimate_total_cost(self, skills: list[SkillMetadata]) -> str:
        """Estimate total cost of skill execution."""
        cost_map = {"low": 1, "medium": 2, "high": 3}
        total = sum(cost_map.get(s.cost_estimate, 1) for s in skills)

        if total <= 2:
            return "low"
        elif total <= 5:
            return "medium"
        else:
            return "high"
