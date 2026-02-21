"""Tests for base selector module - covering missing lines."""

import pytest
from unittest.mock import Mock

from core.skills.selector import SkillSelector, SkillMetadata




class TestSkillSelector:
    """Test SkillSelector base class."""

    def test_resolve_dependencies(self, db):
        """Test dependency resolution adds missing dependencies."""
        selector = SkillSelector(db)
        
        # Create mock skills with dependencies
        from core.skills.selector import SkillMetadata
        skill_b = SkillMetadata(
            name="skill_b", version="1.0.0", description="Dependency",
            category="test", subcategory="sub", triggers=[],
            dependencies=[], priority=5, cost_estimate="low"
        )
        skill_a = SkillMetadata(
            name="skill_a", version="1.0.0", description="Main skill",
            category="test", subcategory="sub", triggers=[],
            dependencies=["skill_b"], priority=5, cost_estimate="low"
        )
        
        # Add to selector
        selector.skills["skill_a"] = skill_a
        selector.skills["skill_b"] = skill_b
        
        skills = [skill_a]
        
        result = selector._resolve_dependencies(skills)
        
        # Should add skill_b at the beginning
        assert len(result) == 2
        assert result[0].name == "skill_b"
        assert result[1].name == "skill_a"

    def test_resolve_dependencies_no_duplicates(self, db):
        """Test dependency resolution doesn't add duplicates."""
        selector = SkillSelector(db)
        
        # Create skills manually
        from core.skills.selector import SkillMetadata
        skill_b = SkillMetadata(
            name="skill_b", version="1.0.0", description="Dependency",
            category="test", subcategory="sub", triggers=[],
            dependencies=[], priority=5, cost_estimate="low"
        )
        skill_a = SkillMetadata(
            name="skill_a", version="1.0.0", description="Main skill",
            category="test", subcategory="sub", triggers=[],
            dependencies=["skill_b"], priority=5, cost_estimate="low"
        )
        
        # Add to selector
        selector.skills["skill_a"] = skill_a
        selector.skills["skill_b"] = skill_b
        
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
        
        # Register test skill
        from core.skills.selector import SkillMetadata
        skill = SkillMetadata(
            name="skill_a", version="1.0.0", description="Skill A",
            category="test", subcategory="sub", triggers=[],
            dependencies=[], priority=5, cost_estimate="low"
        )
        selector.skills["skill_a"] = skill
        
        found_skill = selector.get_skill_by_name("skill_a")
        
        assert found_skill is not None
        assert found_skill.name == "skill_a"
        assert found_skill.description == "Skill A"

    def test_get_skill_by_name_not_found(self, db):
        """Test getting non-existent skill returns None."""
        selector = SkillSelector(db)
        
        skill = selector.get_skill_by_name("nonexistent")
        
        assert skill is None

    def test_list_skills_by_category(self, db):
        """Test listing skills by category."""
        selector = SkillSelector(db)
        
        # Register test skills
        from core.skills.selector import SkillMetadata
        skills = [
            SkillMetadata(name="skill_a", version="1.0.0", description="A", category="test", subcategory="sub", triggers=[], dependencies=[], priority=5, cost_estimate="low"),
            SkillMetadata(name="skill_b", version="1.0.0", description="B", category="test", subcategory="sub", triggers=[], dependencies=[], priority=5, cost_estimate="low"),
            SkillMetadata(name="skill_c", version="1.0.0", description="C", category="code", subcategory="sub", triggers=[], dependencies=[], priority=5, cost_estimate="low")
        ]
        for skill in skills:
            selector.skills[skill.name] = skill
        
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
