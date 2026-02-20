"""Unit tests for SandboxCleaner four-tier strategy."""

from unittest.mock import Mock, patch, call
from datetime import datetime, timezone, timedelta
import pytest
from core.sandbox.cleanup import SandboxCleaner


@pytest.fixture
def mock_db():
    return Mock()


@pytest.fixture
def cleaner(mock_db):
    with patch("core.sandbox.cleanup.Sandbox"):
        return SandboxCleaner(db=mock_db, source_db="test_db")


class TestSandboxCleaner:
    def test_find_closed_session_sandboxes(self, cleaner, mock_db):
        row = Mock()
        row._mapping = {"sandbox_name": "sandbox_closed"}
        mock_db.execute.return_value = [row]

        result = cleaner._find_closed_session_sandboxes()
        assert result == ["sandbox_closed"]

    def test_find_closed_session_sandboxes_error(self, cleaner, mock_db):
        mock_db.execute.side_effect = RuntimeError("boom")
        assert cleaner._find_closed_session_sandboxes() == []

    def test_find_zombie_session_sandboxes(self, cleaner, mock_db):
        row = Mock()
        row._mapping = {"sandbox_name": "sandbox_zombie"}
        mock_db.execute.return_value = [row]

        cutoff = datetime.now(timezone.utc) - timedelta(hours=24)
        result = cleaner._find_zombie_session_sandboxes(cutoff)
        assert result == ["sandbox_zombie"]

    def test_find_expired_unbound(self, cleaner, mock_db):
        row = Mock()
        row._mapping = {"sandbox_name": "sandbox_old"}
        mock_db.execute.return_value = [row]

        cutoff = datetime.now(timezone.utc) - timedelta(hours=24)
        result = cleaner._find_expired_unbound(cutoff)
        assert result == ["sandbox_old"]

    def test_find_orphan_databases(self, cleaner, mock_db):
        # First call: SHOW DATABASES
        db_rows = [("sandbox_orphan",), ("other_db",), ("code_exec_x",)]
        # Second call: metadata query
        meta_row = Mock()
        meta_row._mapping = {"sandbox_name": "code_exec_x"}
        mock_db.execute.side_effect = [db_rows, [meta_row]]

        result = cleaner._find_orphan_databases()
        assert result == ["sandbox_orphan"]

    def test_run_calls_all_tiers(self, cleaner):
        with patch.object(cleaner, "_find_closed_session_sandboxes", return_value=["s1"]), \
             patch.object(cleaner, "_find_zombie_session_sandboxes", return_value=["s2"]), \
             patch.object(cleaner, "_find_expired_unbound", return_value=[]), \
             patch.object(cleaner, "_find_orphan_databases", return_value=["s3"]):
            result = cleaner.run(ttl_hours=24)
            # s1, s2 via _try_delete; s3 via _try_force_delete
            sb = cleaner.sandbox
            assert sb.delete.call_count == 3
            sb.delete.assert_any_call("s1")
            sb.delete.assert_any_call("s2")
            sb.delete.assert_any_call("s3", force=True)
            assert result["cleaned"] == 3

    def test_run_counts_failures(self, cleaner):
        cleaner.sandbox.delete.side_effect = RuntimeError("partial")
        with patch.object(cleaner, "_find_closed_session_sandboxes", return_value=["s1"]), \
             patch.object(cleaner, "_find_zombie_session_sandboxes", return_value=[]), \
             patch.object(cleaner, "_find_expired_unbound", return_value=[]), \
             patch.object(cleaner, "_find_orphan_databases", return_value=[]):
            result = cleaner.run()
            assert result["failed"] == 1
            assert result["cleaned"] == 0
