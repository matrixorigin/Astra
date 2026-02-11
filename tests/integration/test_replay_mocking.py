"""Integration tests for Side-Effect Isolation (Replay Mocking)."""

import pytest
import json
from unittest.mock import Mock, patch
from core.skills.mocking import ToolMockingLayer, MockMode, SecurityError
from core.skills.base import (
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
    RepoType,
    AccessScope,
    SideEffectCategory,
    SideEffectProfile,
)
from sdk import Database


# ============================================================================
# Mock Skills for Testing
# ============================================================================


class ReadSkillInput(SkillInput):
    """Input for read skill"""

    query: str


class ReadSkillOutput(SkillOutput):
    """Output for read skill"""

    data: str


class ReadSkill(Skill):
    """Mock READ skill (safe to replay)"""

    name = "test_read_skill"
    version = "1.0.0"
    description = "Test read skill"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.READ, external_apis=["test_api"]
    )

    def validate_input(self, input_data: dict) -> ReadSkillInput:
        return ReadSkillInput(**input_data)

    def execute(self, input: ReadSkillInput) -> ReadSkillOutput:
        return ReadSkillOutput(
            success=True, result=f"Read: {input.query}", data=f"Read: {input.query}"
        )


class WriteSkill(Skill):
    """Mock WRITE skill (has side-effects)"""

    name = "test_write_skill"
    version = "1.0.0"
    description = "Test write skill"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.WRITE, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.WRITE, external_apis=["test_api"]
    )

    def validate_input(self, input_data: dict) -> SkillInput:
        return SkillInput(**input_data)

    def execute(self, input: SkillInput) -> SkillOutput:
        return SkillOutput(success=True, result="Write executed")


class DestructiveSkill(Skill):
    """Mock DESTRUCTIVE skill (dangerous)"""

    name = "test_destructive_skill"
    version = "1.0.0"
    description = "Test destructive skill"
    requirements = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.ADMIN, llm_required=False
    )
    side_effect_profile = SideEffectProfile(
        category=SideEffectCategory.DESTRUCTIVE, external_apis=["test_api"]
    )

    def validate_input(self, input_data: dict) -> SkillInput:
        return SkillInput(**input_data)

    def execute(self, input: SkillInput) -> SkillOutput:
        return SkillOutput(success=True, result="Destructive executed")


# ============================================================================
# Fixtures
# ============================================================================


@pytest.fixture
def db():
    """Database fixture"""
    return Database()


@pytest.fixture
def session_id():
    """Test session ID"""
    return "test_session_123"


@pytest.fixture
def setup_recorded_result(db, session_id):
    """Setup a recorded skill result in database"""

    def _setup(skill_name: str, params: dict, result: dict):
        # Compute params hash
        import hashlib

        params_str = json.dumps(params, sort_keys=True)
        params_hash = hashlib.sha256(params_str.encode()).hexdigest()[:16]

        # Create a mock event with recorded result
        metadata = {
            "skill_name": skill_name,
            "skill_params": params,
            "skill_params_hash": params_hash,
            "skill_result": result,
        }

        db.execute(
            """
            INSERT INTO conversation_events
            (event_id, user_id, session_id, agent_id, agent_version, 
             event_type, content, skill_name, metadata, created_at)
            VALUES (%s, %s, %s, %s, %s, %s, %s, %s, %s, NOW())
        """,
            (
                "test_event_123",
                "test_user",
                session_id,
                "test_agent",
                "1.0.0",
                "tool_result",
                "Test content",
                skill_name,
                json.dumps(metadata),
            ),
        )

    yield _setup

    # Cleanup
    db.execute("DELETE FROM conversation_events WHERE session_id = %s", (session_id,))


# ============================================================================
# Tests
# ============================================================================


def test_production_mode_executes_skill(db, session_id):
    """Production mode executes skill normally"""
    mock_layer = ToolMockingLayer(MockMode.PRODUCTION, db)
    skill = ReadSkill()

    params = {"user_id": "test", "session_id": session_id, "query": "test"}
    result = mock_layer.execute(skill, params, session_id)

    assert result.success is True
    assert result.data == "Read: test"


def test_replay_mode_uses_recorded_result(db, session_id, setup_recorded_result):
    """Replay mode returns recorded result without execution"""
    # Setup recorded result
    params = {"user_id": "test", "session_id": session_id}
    recorded_result = {"success": True, "result": "Recorded write", "error": None}
    setup_recorded_result("test_write_skill", params, recorded_result)

    # Create mock layer in replay mode
    mock_layer = ToolMockingLayer(MockMode.REPLAY, db)
    skill = WriteSkill()

    # Mock the execute method to track if it's called
    with patch.object(skill, "execute") as mock_execute:
        result = mock_layer.execute(skill, params, session_id)

        # Should return recorded result
        assert result == recorded_result

        # Should NOT call real execute
        mock_execute.assert_not_called()


def test_replay_mode_blocks_destructive_operations(db, session_id):
    """Replay mode blocks destructive operations"""
    mock_layer = ToolMockingLayer(MockMode.REPLAY, db)
    skill = DestructiveSkill()

    params = {"user_id": "test", "session_id": session_id}

    with pytest.raises(SecurityError, match="Blocked destructive skill"):
        mock_layer.execute(skill, params, session_id)


def test_replay_mode_fallback_for_read_operations(db, session_id):
    """Replay mode allows READ operations to execute (fallback)"""
    mock_layer = ToolMockingLayer(MockMode.REPLAY, db)
    skill = ReadSkill()

    params = {"user_id": "test", "session_id": session_id, "query": "test"}
    result = mock_layer.execute(skill, params, session_id)

    # READ operations can execute even in replay mode
    assert result.success is True
    assert result.data == "Read: test"


def test_replay_mode_fallback_when_no_recorded_result(db, session_id):
    """Replay mode falls back to execution when no recorded result found"""
    mock_layer = ToolMockingLayer(MockMode.REPLAY, db)
    skill = WriteSkill()

    params = {"user_id": "test", "session_id": session_id}

    # No recorded result, should fall back to real execution
    result = mock_layer.execute(skill, params, session_id)

    assert result.success is True
    assert result.result == "Write executed"


def test_params_hash_matching(db, session_id, setup_recorded_result):
    """Recorded results are matched by params hash"""
    # Setup recorded result with specific params
    params1 = {"user_id": "test", "session_id": session_id, "value": 123}
    recorded_result = {"success": True, "result": "Recorded", "error": None}
    setup_recorded_result("test_write_skill", params1, recorded_result)

    mock_layer = ToolMockingLayer(MockMode.REPLAY, db)
    skill = WriteSkill()

    # Same params should match
    result1 = mock_layer.execute(skill, params1, session_id)
    assert result1 == recorded_result

    # Different params should not match (fallback to execution)
    params2 = {"user_id": "test", "session_id": session_id, "value": 456}
    with patch.object(skill, "execute") as mock_execute:
        mock_execute.return_value = SkillOutput(success=True, result="New execution")
        result2 = mock_layer.execute(skill, params2, session_id)
        mock_execute.assert_called_once()


def test_dangerous_fallback_warning(db, session_id, caplog):
    """WRITE operations without recorded results trigger dangerous fallback warning"""
    import logging

    caplog.set_level(logging.WARNING)

    mock_layer = ToolMockingLayer(MockMode.REPLAY, db)
    skill = WriteSkill()

    params = {"user_id": "test", "session_id": session_id}

    # No recorded result, should log dangerous warning
    result = mock_layer.execute(skill, params, session_id)

    # Check warning was logged
    assert any("DANGEROUS" in record.message for record in caplog.records)
    assert any("WRITE skill" in record.message for record in caplog.records)


def test_concurrency_warning_without_parent_event_id(db, session_id, caplog):
    """Fuzzy lookup without parent_event_id triggers concurrency warning"""
    import logging

    caplog.set_level(logging.WARNING)

    mock_layer = ToolMockingLayer(MockMode.REPLAY, db)
    skill = WriteSkill()

    params = {"user_id": "test", "session_id": session_id}

    # No parent_event_id, should log concurrency warning
    mock_layer.execute(skill, params, session_id, parent_event_id=None)

    # Check warning was logged
    assert any("fuzzy lookup" in record.message.lower() for record in caplog.records)
    assert any("concurrency" in record.message.lower() for record in caplog.records)


def test_params_hash_consistency(db):
    """Params hash is consistent across different dict orderings"""
    from core.skills.mocking import ToolMockingLayer

    mock_layer = ToolMockingLayer(MockMode.PRODUCTION, db)

    # Same params, different order
    params1 = {"a": 1, "b": 2, "c": 3}
    params2 = {"c": 3, "a": 1, "b": 2}
    params3 = {"b": 2, "c": 3, "a": 1}

    hash1 = mock_layer._hash_params(params1)
    hash2 = mock_layer._hash_params(params2)
    hash3 = mock_layer._hash_params(params3)

    # All hashes should be identical
    assert hash1 == hash2 == hash3

    # Nested dicts should also be consistent
    nested1 = {"outer": {"inner": {"a": 1, "b": 2}}}
    nested2 = {"outer": {"inner": {"b": 2, "a": 1}}}

    hash_nested1 = mock_layer._hash_params(nested1)
    hash_nested2 = mock_layer._hash_params(nested2)

    assert hash_nested1 == hash_nested2
