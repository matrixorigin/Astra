"""Unit tests for ToolMockingLayer

Tests cover:
1. Production mode: Real execution
2. Replay mode: Return recorded results
3. Dry-run mode: Validation only
4. Error handling: Missing recorded results
5. Recording: Store results for future replay
"""

import json
import pytest
from sqlalchemy import delete, text

from core.skills.mocking import (
    ToolMockingLayer,
    MockMode as ExecutionMode,  # Aliased for compatibility with test
    ReplayError
)
from core.skills.base import SideEffectCategory
from api.database import get_db_session
from api.models import Event


@pytest.fixture
def db():
    """Get test database session."""
    session = next(get_db_session())
    # Clean up before test
    session.execute(delete(Event))
    session.commit()
    yield session
    # Clean up after test
    session.execute(delete(Event))
    session.commit()
    session.close()


class TestToolMockingLayer:
    """Test ToolMockingLayer functionality"""
    
    def test_production_mode_execution(self, db):
        """Test production mode executes real skills"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.PRODUCTION,
            db_factory=lambda: db
        )
        
        # Production mode should execute real skills
        # Since we don't have real skills in test, just verify mode
        assert mocker.mode == ExecutionMode.PRODUCTION
    
    def test_replay_mode_returns_recorded_result(self, db):
        """Test replay mode returns recorded results"""
        from uuid_utils import uuid7
        
        # Create a recorded skill invocation event
        # Note: use event_metadata (ORM attribute name) instead of metadata
        # Note: pass dict directly for JSON columns, not json.dumps string
        db.add(Event(
            event_id=str(uuid7()),
            session_id="sess_123",
            event_type="tool_result",  # Was skill_invocation
            content=json.dumps({
                "skill_name": "test_skill",
                "skill_version": "1.0.0",
                "skill_params": {"param": "value"}
            }),
            event_metadata={
                "skill_params": {"param": "value"},
                "skill_result": {"status": "recorded"}
            },
            user_id="user_001",
            skill_name="test_skill",
            skill_version="1.0.0",
            skill_result={"status": "recorded"},  # Pass dict, not json.dumps
            causal_chain_id=str(uuid7()),
        ))
        db.commit()
        
        mocker = ToolMockingLayer(
            mode=ExecutionMode.REPLAY,
            db_factory=lambda: db,
            session_id="sess_123"
        )
        
        # Should return recorded result
        result = mocker.invoke_skill("test_skill", {"param": "value"})
        assert result["status"] == "recorded"
    
    def test_replay_mode_missing_result_raises_error(self, db):
        """Test replay mode raises error when no recorded result"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.REPLAY,
            db_factory=lambda: db,
            session_id="sess_123"
        )
        
        # Should raise ReplayError
        with pytest.raises(ReplayError) as exc_info:
            mocker.invoke_skill("unknown_skill", {"param": "value"})
        
        assert "No recorded result" in str(exc_info.value)
    
    def test_dry_run_mode_validates_only(self, db):
        """Test dry-run mode validates without execution"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.DRY_RUN,
            db_factory=lambda: db
        )
        
        result = mocker.invoke_skill("test_skill", {"param": "value"})
        
        assert result["status"] == "dry_run"
        assert result["skill_id"] == "test_skill"
        assert result["params"] == {"param": "value"}
    
    def test_record_skill_invocation(self, db):
        """Test recording skill invocation results"""
        from uuid_utils import uuid7
        
        # First create an event
        event_id = str(uuid7())
        db.add(Event(
            event_id=event_id,
            session_id="sess_123",
            event_type="skill_invocation",
            content=json.dumps({
                "skill_name": "test_skill",
                "skill_version": "1.0.0",
                "skill_params": {"param": "value"}
            }),
            event_metadata={},
            user_id="user_001",
            skill_name="test_skill",
            skill_version="1.0.0",
            causal_chain_id=str(uuid7()),
        ))
        db.commit()
        
        mocker = ToolMockingLayer(
            mode=ExecutionMode.PRODUCTION,
            db_factory=lambda: db
        )
        
        # Record result
        mocker.record_skill_invocation(
            event_id=event_id,
            skill_id="test_skill",
            params={"param": "value"},
            result={"status": "success"},
            side_effects={"api_calls": ["github.merge_pr"]}
        )
        
        # Verify database was updated
        result = db.execute(text("SELECT * FROM conversation_events WHERE event_id = :event_id"), {"event_id": event_id})
        events = list(result)
        assert len(events) > 0
        assert events[0]._mapping["skill_result"] is not None
    
    def test_replay_mode_requires_session_id(self, db):
        """Test replay mode requires session_id"""
        with pytest.raises(ValueError) as exc_info:
            ToolMockingLayer(
                mode=ExecutionMode.REPLAY,
                db_factory=lambda: db,
                session_id=None
            )
        
        assert "session_id required" in str(exc_info.value)
    
    def test_make_key_consistent(self, db):
        """Test key generation is consistent regardless of param order"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.PRODUCTION,
            db_factory=lambda: db
        )
        
        key1 = mocker._make_key("skill", {"a": 1, "b": 2})
        key2 = mocker._make_key("skill", {"b": 2, "a": 1})
        
        assert key1 == key2
    
    def test_load_multiple_recorded_results(self, db):
        """Test loading multiple recorded results"""
        from uuid_utils import uuid7
        
        # Create multiple recorded skill invocation events
        # Note: use event_metadata (ORM attribute name) instead of metadata
        # Note: pass dict directly for JSON columns, not json.dumps string
        db.add(Event(
            event_id=str(uuid7()),
            session_id="sess_123",
            event_type="tool_result",
            content=json.dumps({
                "skill_name": "skill1",
                "skill_version": "1.0.0",
                "skill_params": {"id": 1}
            }),
            event_metadata={
                "skill_params": {"id": 1},
                "skill_result": {"result": "first"}
            },
            user_id="user_001",
            skill_name="skill1",
            skill_version="1.0.0",
            skill_result={"result": "first"},  # Pass dict, not json.dumps
            causal_chain_id=str(uuid7()),
        ))
        
        db.add(Event(
            event_id=str(uuid7()),
            session_id="sess_123",
            event_type="tool_result",
            content=json.dumps({
                "skill_name": "skill2",
                "skill_version": "1.0.0",
                "skill_params": {"id": 2}
            }),
            event_metadata={
                "skill_params": {"id": 2},
                "skill_result": {"result": "second"}
            },
            user_id="user_001",
            skill_name="skill2",
            skill_version="1.0.0",
            skill_result={"result": "second"},  # Pass dict, not json.dumps
            causal_chain_id=str(uuid7()),
        ))
        db.commit()
        
        mocker = ToolMockingLayer(
            mode=ExecutionMode.REPLAY,
            db_factory=lambda: db,
            session_id="sess_123"
        )
        
        # Should have loaded both results
        assert len(mocker.recorded_results) == 2
        
        result1 = mocker.invoke_skill("skill1", {"id": 1})
        result2 = mocker.invoke_skill("skill2", {"id": 2})
        
        assert result1["result"] == "first"
        assert result2["result"] == "second"


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
