"""Tests for base selector module - covering missing lines."""

import pytest
from unittest.mock import Mock

from core.skills.selector import SkillSelector, SkillMetadata, SkillOrchestrator
from sdk import Database


@pytest.fixture
def db():
    """Mock database."""
    mock_db = Mock(spec=Database)
    mock_db.fetchall.return_value = [
        {
            "skill_name": "skill_a",
            "version": "1.0.0",
            "description": "Skill A",
            "category": "test",
            "subcategory": "sub1",
            "triggers": '["trigger_a"]',
            "dependencies": '["skill_b"]',
            "priority": 8,
            "cost_estimate": "low",
        },
        {
            "skill_name": "skill_b",
            "version": "1.0.0",
            "description": "Skill B",
            "category": "test",
            "subcategory": "sub1",
            "triggers": '["trigger_b"]',
            "dependencies": '[]',
            "priority": 5,
            "cost_estimate": "medium",
        },
        {
            "skill_name": "skill_c",
            "version": "1.0.0",
            "description": "Skill C",
            "category": "code",
            "subcategory": "analysis",
            "triggers": '["trigger_c"]',
            "dependencies": '[]',
            "priority": 6,
            "cost_estimate": "high",
        },
    ]
    return mock_db


class TestSkillSelector:
    """Test SkillSelector base class."""

    def test_resolve_dependencies(self, db):
        """Test dependency resolution adds missing dependencies."""
        selector = SkillSelector(db)
        
        # skill_a depends on skill_b
        skill_a = selector.skills["skill_a"]
        skills = [skill_a]
        
        result = selector._resolve_dependencies(skills)
        
        # Should add skill_b at the beginning
        assert len(result) == 2
        assert result[0].name == "skill_b"
        assert result[1].name == "skill_a"

    def test_resolve_dependencies_no_duplicates(self, db):
        """Test dependency resolution doesn't add duplicates."""
        selector = SkillSelector(db)
        
        skill_a = selector.skills["skill_a"]
        skill_b = selector.skills["skill_b"]
        skills = [skill_a, skill_b]
        
        result = selector._resolve_dependencies(skills)
        
        # Should not duplicate skill_b
        assert len(result) == 2
        names = [s.name for s in result]
        assert names.count("skill_b") == 1

    def test_resolve_dependencies_missing_dep(self, db):
        """Test dependency resolution handles missing dependencies."""
        selector = SkillSelector(db)
        
        # Create skill with non-existent dependency
        skill = SkillMetadata(
            name="test_skill",
            version="1.0.0",
            description="Test",
            category="test",
            subcategory="sub",
            triggers=[],
            dependencies=["nonexistent_skill"],
            priority=5,
            cost_estimate="low"
        )
        
        result = selector._resolve_dependencies([skill])
        
        # Should not crash, just skip missing dependency
        assert len(result) == 1
        assert result[0].name == "test_skill"

    def test_get_skill_by_name(self, db):
        """Test getting skill by name."""
        selector = SkillSelector(db)
        
        skill = selector.get_skill_by_name("skill_a")
        
        assert skill is not None
        assert skill.name == "skill_a"
        assert skill.description == "Skill A"

    def test_get_skill_by_name_not_found(self, db):
        """Test getting non-existent skill returns None."""
        selector = SkillSelector(db)
        
        skill = selector.get_skill_by_name("nonexistent")
        
        assert skill is None

    def test_list_skills_by_category(self, db):
        """Test listing skills by category."""
        selector = SkillSelector(db)
        
        test_skills = selector.list_skills_by_category("test")
        
        assert len(test_skills) == 2
        assert all(s.category == "test" for s in test_skills)
        
        code_skills = selector.list_skills_by_category("code")
        
        assert len(code_skills) == 1
        assert code_skills[0].name == "skill_c"

    def test_list_skills_by_category_empty(self, db):
        """Test listing skills for non-existent category."""
        selector = SkillSelector(db)
        
        skills = selector.list_skills_by_category("nonexistent")
        
        assert skills == []


class TestSkillOrchestrator:
    """Test SkillOrchestrator."""

    def test_plan_execution_with_skills(self, db):
        """Test execution planning with selected skills."""
        orchestrator = SkillOrchestrator(db)
        
        plan = orchestrator.plan_execution("trigger_a trigger_b")
        
        assert "skills" in plan
        assert "execution_order" in plan
        assert "estimated_cost" in plan
        assert len(plan["skills"]) > 0
        assert plan["dependencies_resolved"] is True

    def test_plan_execution_no_skills(self, db):
        """Test execution planning with no matching skills."""
        orchestrator = SkillOrchestrator(db)
        
        plan = orchestrator.plan_execution("unrelated query xyz")
        
        assert plan["skills"] == []
        assert plan["execution_order"] == []
        assert plan["estimated_cost"] == "none"

    def test_estimate_total_cost_low(self, db):
        """Test cost estimation - low."""
        orchestrator = SkillOrchestrator(db)
        
        skills = [
            SkillMetadata(
                name="s1", version="1.0.0", description="", category="test",
                subcategory="sub", triggers=[], dependencies=[],
                priority=5, cost_estimate="low"
            )
        ]
        
        cost = orchestrator._estimate_total_cost(skills)
        
        assert cost == "low"

    def test_estimate_total_cost_medium(self, db):
        """Test cost estimation - medium."""
        orchestrator = SkillOrchestrator(db)
        
        skills = [
            SkillMetadata(
                name="s1", version="1.0.0", description="", category="test",
                subcategory="sub", triggers=[], dependencies=[],
                priority=5, cost_estimate="low"
            ),
            SkillMetadata(
                name="s2", version="1.0.0", description="", category="test",
                subcategory="sub", triggers=[], dependencies=[],
                priority=5, cost_estimate="medium"
            )
        ]
        
        cost = orchestrator._estimate_total_cost(skills)
        
        assert cost == "medium"

    def test_estimate_total_cost_high(self, db):
        """Test cost estimation - high."""
        orchestrator = SkillOrchestrator(db)
        
        skills = [
            SkillMetadata(
                name="s1", version="1.0.0", description="", category="test",
                subcategory="sub", triggers=[], dependencies=[],
                priority=5, cost_estimate="high"
            ),
            SkillMetadata(
                name="s2", version="1.0.0", description="", category="test",
                subcategory="sub", triggers=[], dependencies=[],
                priority=5, cost_estimate="high"
            )
        ]
        
        cost = orchestrator._estimate_total_cost(skills)
        
        assert cost == "high"

    def test_plan_execution_includes_skill_details(self, db):
        """Test plan includes all skill details."""
        orchestrator = SkillOrchestrator(db)
        
        plan = orchestrator.plan_execution("trigger_a")
        
        if plan["skills"]:
            skill = plan["skills"][0]
            assert "name" in skill
            assert "description" in skill
            assert "category" in skill
            assert "priority" in skill
