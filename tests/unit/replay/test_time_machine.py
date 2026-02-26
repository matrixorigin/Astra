"""Unit tests for TimeMachine — validation and delegation logic."""

import pytest
from unittest.mock import Mock, patch

from core.replay.time_machine import TimeMachine, _validate_checkpoint_name


class TestValidateCheckpointName:
    def test_accepts_valid(self):
        for name in ["snap1", "my-checkpoint", "v2_backup", "ABC123"]:
            _validate_checkpoint_name(name)  # no raise

    def test_rejects_injection(self):
        for bad in ["'; DROP TABLE --", "a b", "foo'bar", "x;y", "{bad}", "a\nb"]:
            with pytest.raises(ValueError, match="Invalid checkpoint name"):
                _validate_checkpoint_name(bad)


class TestTimeMachineUnit:
    """Test TimeMachine methods with mocked dependencies."""

    @pytest.fixture
    def tm(self):
        mock_db = Mock()
        tm = TimeMachine(lambda: mock_db)
        tm.git = Mock()
        tm.reader = Mock()
        return tm

    def test_create_checkpoint_validates_name(self, tm):
        with pytest.raises(ValueError):
            tm.create_checkpoint("bad name!")

    def test_create_checkpoint_delegates(self, tm):
        tm.git.create_snapshot.return_value = {"timestamp": "2026-01-01T00:00:00Z"}
        result = tm.create_checkpoint("snap1", description="test")
        tm.git.create_snapshot.assert_called_once_with("snap1")
        assert result["checkpoint_name"] == "snap1"
        assert result["description"] == "test"

    def test_restore_validates_name(self, tm):
        with pytest.raises(ValueError):
            tm.restore_to_checkpoint("'; DROP TABLE")

    def test_restore_delegates(self, tm):
        tm.restore_to_checkpoint("snap1")
        tm.git.restore_from_snapshot.assert_called_once_with("snap1")

    def test_list_checkpoints_delegates(self, tm):
        tm.git.list_snapshots.return_value = [{"name": "snap1"}]
        result = tm.list_checkpoints()
        assert result == [{"name": "snap1"}]

    def test_replay_conversation_validates_name(self, tm):
        with pytest.raises(ValueError):
            tm.replay_conversation("sess1", "bad name!")
