"""Internal skill selector implementation.

⚠️ DO NOT USE DIRECTLY - Use SkillPipeline instead.

This module contains internal implementation details used by ModernSkillSelector:
- SkillMetadata: Core data structure for skill information
- SkillSelector: Rule-based retrieval engine

External code should use SkillPipeline from core.skills.pipeline.
"""

from dataclasses import dataclass

from sqlalchemy.orm import Session

from api.database import get_db_session
from api.models import SkillRegistry as SkillModel
from core.logging_config import get_logger

logger = get_logger(__name__)


@dataclass
class SkillMetadata:
    """Skill metadata with progressive disclosure support.

    Tier 1 (embedding index): name + description + triggers → vector.
           Lives in SkillIndex, never in LLM context. 0 prompt tokens.
    Tier 2 (full schema):     complete OpenAI tool JSON schema loaded
           into LLM context, budget-controlled. Token cost measured at runtime.
    """

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

    def _get_session(self) -> Session:
        if self._session is None:
            self._session = next(get_db_session())
        return self._session

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()

    def close(self):
        if self._owns_session and self._session:
            self._session.close()

    def __del__(self):
        self.close()

    def _load_skills(self):
        """Load skills with metadata from database."""
        self.skills = {}
        
        session = self._get_session()
        skills_data = session.query(SkillModel).filter(SkillModel.is_active == 1).all()

        for skill in skills_data:
            metadata = SkillMetadata(
                name=skill.skill_name,
                version=skill.version,
                description=skill.description or "",
                category=skill.category or "general",
                subcategory=skill.subcategory or "default",
                triggers=skill.triggers or [],
                dependencies=skill.dependencies or [],
                priority=skill.priority or 5,
                cost_estimate=skill.cost_estimate or "medium",
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
        """Calculate match score for skill using token-level matching.

        Combines word-level Jaccard similarity with substring containment.
        Jaccard reduces false positives from partial substring hits while
        substring matching handles stemming (e.g. "bug" in "bugs").
        """
        score = 0.0
        query_words = set(query.split())

        for trigger in skill.triggers:
            trigger_lower = trigger.lower()
            trigger_words = set(trigger_lower.split("_"))
            trigger_words.add(trigger_lower)

            # Token-level: word intersection
            overlap = query_words & trigger_words
            if overlap:
                jaccard = len(overlap) / len(query_words | trigger_words)
                score += jaccard + (0.5 if trigger_lower in query else 0)
            elif trigger_lower in query:
                # Substring fallback: trigger appears inside a query word
                # (e.g. "bug" in "bugs"), weaker signal than exact word match
                score += 0.5
            elif any(trigger_lower in w for w in query_words):
                # Trigger is a substring of a query word
                score += 0.3

        # Boost by priority
        score *= skill.priority / 10.0

        return score

    def _resolve_dependencies(self, skills: list[SkillMetadata]) -> list[SkillMetadata]:
        """Resolve transitive dependencies with topological sort and cycle detection."""
        # Collect all needed skills (BFS for transitive deps)
        needed: dict[str, SkillMetadata] = {s.name: s for s in skills}
        queue = list(skills)
        while queue:
            skill = queue.pop(0)
            for dep_name in skill.dependencies:
                if dep_name not in needed and dep_name in self.skills:
                    dep = self.skills[dep_name]
                    needed[dep_name] = dep
                    queue.append(dep)

        # Topological sort (Kahn's algorithm)
        in_degree: dict[str, int] = {name: 0 for name in needed}
        for skill in needed.values():
            for dep_name in skill.dependencies:
                if dep_name in needed:
                    in_degree[skill.name] += 1

        queue = [name for name, deg in in_degree.items() if deg == 0]
        ordered: list[SkillMetadata] = []
        while queue:
            name = queue.pop(0)
            ordered.append(needed[name])
            for skill in needed.values():
                if name in skill.dependencies:
                    in_degree[skill.name] -= 1
                    if in_degree[skill.name] == 0:
                        queue.append(skill.name)

        if len(ordered) < len(needed):
            cycle = [n for n in needed if n not in {s.name for s in ordered}]
            logger.warning(f"Circular dependency detected: {cycle}, returning flat list")
            return list(needed.values())

        return ordered

    def get_skill_by_name(self, name: str) -> SkillMetadata | None:
        """Get skill metadata by name."""
        skill = self.skills.get(name)
        return skill if isinstance(skill, SkillMetadata) else None

    def list_skills_by_category(self, category: str) -> list[SkillMetadata]:
        """List skills in a category."""
        return [s for s in self.skills.values() if s.category == category]
