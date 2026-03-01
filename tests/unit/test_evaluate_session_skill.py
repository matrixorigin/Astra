"""Test evaluate_session skill."""

import pytest
from skills.evaluate_session.skill import EvaluateSessionSkill, EvaluateSessionInput


class TestEvaluateSessionSkill:
    """Test session evaluation skill."""
    
    def test_skill_structure(self):
        """Test skill has correct structure."""
        skill = EvaluateSessionSkill()
        
        assert skill.name == "evaluate_session"
        assert skill.description
        assert skill.version == "1.0.0"
    
    def test_evaluate_nonexistent_session(self):
        """Test evaluating a non-existent session."""
        skill = EvaluateSessionSkill()
        input_data = EvaluateSessionInput(
            session_id="00000000-0000-0000-0000-000000000000",
            include_details=False
        )
        
        result = skill.execute(input_data)
        
        assert result.success is False
        assert result.error is not None
        assert "No events found" in result.error
    
    @pytest.mark.skip(reason="Requires real session data in test database")
    def test_evaluate_existing_session(self):
        """Test evaluating a real session (requires test data)."""
        skill = EvaluateSessionSkill()
        input_data = EvaluateSessionInput(
            session_id="019ca9f1-3dc6-72b3-9813-1f38f7349c53",
            include_details=False
        )
        
        result = skill.execute(input_data)
        
        assert result.success is True
        assert result.error is None
        assert result.tokens["total"] > 0
        assert result.assessment["token_efficiency"] in ("excellent", "good", "moderate", "needs_improvement")
