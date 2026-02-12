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
from unittest.mock import Mock, patch

from core.replay.tool_mocking import (
    ToolMockingLayer,
    ExecutionMode,
    SideEffectCategory,
    ReplayError
)


class TestToolMockingLayer:
    """Test ToolMockingLayer functionality"""
    
    @pytest.fixture
    def mock_db(self):
        """Create mock database"""
        db = Mock()
        cursor = Mock()
        cursor.__enter__ = Mock(return_value=cursor)
        cursor.__exit__ = Mock(return_value=False)
        db.get_cursor = Mock(return_value=cursor)
        return db
    
    def test_production_mode_execution(self, mock_db):
        """Test production mode executes real skills"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.PRODUCTION,
            db=mock_db
        )
        
        # Mock real execution
        with patch.object(mocker, '_execute_real', return_value={"status": "success"}):
            result = mocker.invoke_skill("test_skill", {"param": "value"})
        
        assert result["status"] == "success"
    
    def test_replay_mode_returns_recorded_result(self, mock_db):
        """Test replay mode returns recorded results"""
        # Setup mock cursor to return recorded event
        cursor = mock_db.get_cursor().__enter__()
        cursor.fetchall.return_value = [
            {
                "skill_name": "test_skill",
                "skill_version": "1.0.0",
                "metadata": json.dumps({"skill_params": {"param": "value"}}),
                "skill_result": json.dumps({"status": "recorded"})
            }
        ]
        
        mocker = ToolMockingLayer(
            mode=ExecutionMode.REPLAY,
            db=mock_db,
            session_id="sess_123"
        )
        
        # Should return recorded result
        result = mocker.invoke_skill("test_skill", {"param": "value"})
        assert result["status"] == "recorded"
    
    def test_replay_mode_missing_result_raises_error(self, mock_db):
        """Test replay mode raises error when no recorded result"""
        cursor = mock_db.get_cursor().__enter__()
        cursor.fetchall.return_value = []
        
        mocker = ToolMockingLayer(
            mode=ExecutionMode.REPLAY,
            db=mock_db,
            session_id="sess_123"
        )
        
        # Should raise ReplayError
        with pytest.raises(ReplayError) as exc_info:
            mocker.invoke_skill("unknown_skill", {"param": "value"})
        
        assert "No recorded result" in str(exc_info.value)
    
    def test_dry_run_mode_validates_only(self, mock_db):
        """Test dry-run mode validates without execution"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.DRY_RUN,
            db=mock_db
        )
        
        result = mocker.invoke_skill("test_skill", {"param": "value"})
        
        assert result["status"] == "dry_run"
        assert result["skill_id"] == "test_skill"
        assert result["params"] == {"param": "value"}
    
    def test_record_skill_invocation(self, mock_db):
        """Test recording skill invocation results"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.PRODUCTION,
            db=mock_db
        )
        
        # Record result
        mocker.record_skill_invocation(
            event_id="evt_123",
            skill_id="test_skill",
            params={"param": "value"},
            result={"status": "success"},
            side_effects={"api_calls": ["github.merge_pr"]}
        )
        
        # Verify database update
        mock_db.execute.assert_called_once()
        call_args = mock_db.execute.call_args
        assert "UPDATE conversation_events" in call_args[0][0]
        assert call_args[0][1][2] == "evt_123"  # event_id
    
    def test_replay_mode_requires_session_id(self, mock_db):
        """Test replay mode requires session_id"""
        with pytest.raises(ValueError) as exc_info:
            ToolMockingLayer(
                mode=ExecutionMode.REPLAY,
                db=mock_db,
                session_id=None
            )
        
        assert "session_id required" in str(exc_info.value)
    
    def test_make_key_consistent(self, mock_db):
        """Test key generation is consistent regardless of param order"""
        mocker = ToolMockingLayer(
            mode=ExecutionMode.PRODUCTION,
            db=mock_db
        )
        
        key1 = mocker._make_key("skill", {"a": 1, "b": 2})
        key2 = mocker._make_key("skill", {"b": 2, "a": 1})
        
        assert key1 == key2
    
    def test_load_multiple_recorded_results(self, mock_db):
        """Test loading multiple recorded results"""
        cursor = mock_db.get_cursor().__enter__()
        cursor.fetchall.return_value = [
            {
                "skill_name": "skill1",
                "skill_version": "1.0.0",
                "metadata": json.dumps({"skill_params": {"id": 1}}),
                "skill_result": json.dumps({"result": "first"})
            },
            {
                "skill_name": "skill2",
                "skill_version": "1.0.0",
                "metadata": json.dumps({"skill_params": {"id": 2}}),
                "skill_result": json.dumps({"result": "second"})
            }
        ]
        
        mocker = ToolMockingLayer(
            mode=ExecutionMode.REPLAY,
            db=mock_db,
            session_id="sess_123"
        )
        
        # Should have loaded both results
        assert len(mocker.recorded_results) == 2
        
        result1 = mocker.invoke_skill("skill1", {"id": 1})
        result2 = mocker.invoke_skill("skill2", {"id": 2})
        
        assert result1["result"] == "first"
        assert result2["result"] == "second"
