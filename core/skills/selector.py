"""Skill selection and orchestration."""

from typing import List, Dict, Any, Optional
from dataclasses import dataclass
import re

from sdk import Database
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
    triggers: List[str]
    dependencies: List[str]
    priority: int
    cost_estimate: str


class SkillSelector:
    """Rule-based skill selector with keyword matching."""
    
    def __init__(self, db: Database):
        self.db = db
        self._load_skills()
    
    def _load_skills(self):
        """Load skills with metadata from database."""
        self.skills = {}
        
        # Load from database
        rows = self.db.fetchall("""
            SELECT skill_name, version, description, category, subcategory,
                   triggers, dependencies, priority, cost_estimate, is_active
            FROM skills_registry
            WHERE is_active = 1
        """)
        
        for row in rows:
            # Parse JSON fields
            import json
            triggers = json.loads(row['triggers']) if row.get('triggers') else []
            dependencies = json.loads(row['dependencies']) if row.get('dependencies') else []
            
            skill = SkillMetadata(
                name=row['skill_name'],
                version=row['version'],
                description=row['description'],
                category=row.get('category', 'general'),
                subcategory=row.get('subcategory', 'default'),
                triggers=triggers,
                dependencies=dependencies,
                priority=row.get('priority', 5),
                cost_estimate=row.get('cost_estimate', 'medium')
            )
            
            self.skills[skill.name] = skill
        
        logger.info(f"Loaded {len(self.skills)} skills from database")
    
    def select_skills(
        self,
        query: str,
        max_skills: int = 3
    ) -> List[SkillMetadata]:
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
        score *= (skill.priority / 10.0)
        
        return score
    
    def _resolve_dependencies(
        self,
        skills: List[SkillMetadata]
    ) -> List[SkillMetadata]:
        """Resolve skill dependencies."""
        result = list(skills)
        added = set(s.name for s in skills)
        
        for skill in skills:
            for dep_name in skill.dependencies:
                if dep_name not in added and dep_name in self.skills:
                    result.insert(0, self.skills[dep_name])
                    added.add(dep_name)
                    logger.debug(f"Added dependency: {dep_name} for {skill.name}")
        
        return result
    
    def get_skill_by_name(self, name: str) -> Optional[SkillMetadata]:
        """Get skill metadata by name."""
        return self.skills.get(name)
    
    def list_skills_by_category(self, category: str) -> List[SkillMetadata]:
        """List skills in a category."""
        return [s for s in self.skills.values() if s.category == category]


class SkillOrchestrator:
    """Orchestrate skill selection and execution."""
    
    def __init__(self, db: Database):
        self.db = db
        self.selector = SkillSelector(db)
    
    def plan_execution(
        self,
        query: str,
        context: Optional[Dict[str, Any]] = None
    ) -> Dict[str, Any]:
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
            return {
                "skills": [],
                "execution_order": [],
                "estimated_cost": "none"
            }
        
        # Build execution plan
        plan = {
            "skills": [
                {
                    "name": s.name,
                    "description": s.description,
                    "category": s.category,
                    "priority": s.priority
                }
                for s in selected_skills
            ],
            "execution_order": [s.name for s in selected_skills],
            "estimated_cost": self._estimate_total_cost(selected_skills),
            "dependencies_resolved": True
        }
        
        logger.info(f"Execution plan: {plan['execution_order']}")
        
        return plan
    
    def _estimate_total_cost(self, skills: List[SkillMetadata]) -> str:
        """Estimate total cost of skill execution."""
        cost_map = {"low": 1, "medium": 2, "high": 3}
        total = sum(cost_map.get(s.cost_estimate, 1) for s in skills)
        
        if total <= 2:
            return "low"
        elif total <= 5:
            return "medium"
        else:
            return "high"
