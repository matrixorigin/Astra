"""Unit tests for RegressionGate skill change handling."""

import pytest
from unittest.mock import Mock, patch, call
from sqlalchemy.orm import Session
from sqlalchemy import text

from core.evaluation.regression_gate import RegressionGate, ChangeType


@pytest.fixture
def mock_db():
    """Mock database session."""
    db = Mock(spec=Session)
    db.execute = Mock(return_value=Mock())
    db.commit = Mock()
    return db


@pytest.fixture
def regression_gate(mock_db):
    """Create RegressionGate instance with mocked dependencies."""
    with patch("core.evaluation.regression_gate.Sandbox"):
        gate = RegressionGate(lambda: mock_db, account="test")
        return gate


class TestSkillChangeApply:
    """Test skill change application with field mapping."""

    def test_skill_change_with_definition_key(self, regression_gate, mock_db):
        """Test skill change with 'definition' key."""
        change_content = {
            "skill_name": "test_skill",
            "version": "1.0.0",
            "description": "Test skill",
            "definition": {"type": "function", "params": []},
        }

        regression_gate._apply_change_to_sandbox(
            sandbox_name="test_sandbox",
            change_type=ChangeType.SKILL,
            change_id="skill_123",
            change_content=change_content,
        )

        # Verify SQL execution
        assert mock_db.execute.called
        call_args = mock_db.execute.call_args
        params = call_args[0][1]  # Second argument is the params dict

        assert params["skill_id"] == "skill_123"
        assert params["skill_name"] == "test_skill"
        assert params["version"] == "1.0.0"
        assert params["description"] == "Test skill"
        assert params["definition"] == {"type": "function", "params": []}

    def test_skill_change_with_skill_definition_key(self, regression_gate, mock_db):
        """Test skill change with 'skill_definition' key (backward compat)."""
        change_content = {
            "skill_name": "test_skill",
            "version": "2.0.0",
            "skill_definition": {"type": "tool", "name": "test"},
        }

        regression_gate._apply_change_to_sandbox(
            sandbox_name="test_sandbox",
            change_type=ChangeType.SKILL,
            change_id="skill_456",
            change_content=change_content,
        )

        params = mock_db.execute.call_args[0][1]

        # Should use skill_definition when definition is missing
        assert params["definition"] == {"type": "tool", "name": "test"}

    def test_skill_change_with_name_fallback(self, regression_gate, mock_db):
        """Test skill_name fallback: skill_name → name → change_id."""
        # Test with 'name' key
        change_content = {"name": "fallback_skill", "version": "1.0.0", "definition": {}}

        regression_gate._apply_change_to_sandbox(
            sandbox_name="test_sandbox",
            change_type=ChangeType.SKILL,
            change_id="skill_789",
            change_content=change_content,
        )

        params = mock_db.execute.call_args[0][1]
        assert params["skill_name"] == "fallback_skill"

    def test_skill_change_with_change_id_fallback(self, regression_gate, mock_db):
        """Test skill_name fallback to change_id when no name provided."""
        change_content = {"version": "1.0.0", "definition": {}}

        regression_gate._apply_change_to_sandbox(
            sandbox_name="test_sandbox",
            change_type=ChangeType.SKILL,
            change_id="skill_default",
            change_content=change_content,
        )

        params = mock_db.execute.call_args[0][1]
        assert params["skill_name"] == "skill_default"

    def test_skill_change_with_empty_description(self, regression_gate, mock_db):
        """Test skill change with missing description uses empty string."""
        change_content = {"skill_name": "test_skill", "definition": {}}

        regression_gate._apply_change_to_sandbox(
            sandbox_name="test_sandbox",
            change_type=ChangeType.SKILL,
            change_id="skill_no_desc",
            change_content=change_content,
        )

        params = mock_db.execute.call_args[0][1]
        assert params["description"] == ""

    def test_skill_change_with_default_version(self, regression_gate, mock_db):
        """Test skill change with missing version uses default."""
        change_content = {"skill_name": "test_skill", "definition": {}}

        regression_gate._apply_change_to_sandbox(
            sandbox_name="test_sandbox",
            change_type=ChangeType.SKILL,
            change_id="skill_no_version",
            change_content=change_content,
        )

        params = mock_db.execute.call_args[0][1]
        assert params["version"] == "1.0.0"

    def test_skill_change_sql_includes_all_fields(self, regression_gate, mock_db):
        """Test SQL includes all required fields."""
        change_content = {
            "skill_name": "complete_skill",
            "version": "3.0.0",
            "description": "Complete test",
            "definition": {"complete": True},
        }

        regression_gate._apply_change_to_sandbox(
            sandbox_name="test_sandbox",
            change_type=ChangeType.SKILL,
            change_id="skill_complete",
            change_content=change_content,
        )

        sql = mock_db.execute.call_args[0][0].text

        # Check INSERT fields
        assert "skill_id" in sql
        assert "skill_name" in sql
        assert "version" in sql
        assert "description" in sql
        assert "skill_definition" in sql
        assert "is_active" in sql
        assert "created_at" in sql
        assert "updated_at" in sql

        # Check ON DUPLICATE KEY UPDATE
        assert "ON DUPLICATE KEY UPDATE" in sql
        assert "skill_definition = :definition" in sql
        assert "version = :version" in sql
        assert "description = :description" in sql
        assert "is_active = 1" in sql
        assert "updated_at = NOW()" in sql
