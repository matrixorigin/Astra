import pytest
from unittest.mock import MagicMock, patch, Mock
from sqlalchemy import text
from sqlalchemy.orm import Session
from core.git_for_data import GitForData

class TestGitForDataInjection:
    
    @pytest.fixture
    def mock_db_session(self):
        mock = Mock(spec=Session)
        mock.execute = MagicMock()
        return mock

    @pytest.fixture
    def git_for_data(self, mock_db_session):
        return GitForData(lambda: mock_db_session)

    def test_query_at_snapshot_simple_injection(self, git_for_data, mock_db_session):
        """Test simple injection of snapshot clause."""
        git_for_data.query_at_snapshot("SELECT * FROM users", "snap1")
        
        # Verify executed query contains snapshot clause
        call_args = mock_db_session.execute.call_args
        assert call_args is not None
        executed_sql = str(call_args[0][0])
        assert "FROM users {SNAPSHOT = 'snap1'}" in executed_sql

    def test_query_at_snapshot_with_alias(self, git_for_data, mock_db_session):
        """Test injection with table alias."""
        # Current implementation might be fragile here
        git_for_data.query_at_snapshot("SELECT * FROM users u", "snap1")
        
        call_args = mock_db_session.execute.call_args
        executed_sql = str(call_args[0][0])
        # Depending on implementation, it might be FROM users {SNAPSHOT = 'snap1'} u or FROM users u {SNAPSHOT = 'snap1'}
        # MatrixOne documentation says: FROM table_name {SNAPSHOT = ...} [AS alias]
        # But my regex replaces "FROM users" -> "FROM users {SNAPSHOT...}"
        # So "FROM users u" becomes "FROM users {SNAPSHOT...} u" which is valid if alias comes after
        assert "FROM users {SNAPSHOT = 'snap1'} u" in executed_sql

    def test_query_at_snapshot_multiple_tables(self, git_for_data, mock_db_session):
        """Test injection with multiple tables (JOINs)."""
        query = "SELECT * FROM users u JOIN orders o ON u.id = o.user_id"
        git_for_data.query_at_snapshot(query, "snap1")
        
        call_args = mock_db_session.execute.call_args
        executed_sql = str(call_args[0][0])
        assert "FROM users {SNAPSHOT = 'snap1'} u" in executed_sql
        assert "JOIN orders {SNAPSHOT = 'snap1'} o" in executed_sql

    def test_query_at_snapshot_complex_spacing(self, git_for_data, mock_db_session):
        """Test injection with irregular whitespace."""
        query = "SELECT * FROM   users   WHERE id=1"
        git_for_data.query_at_snapshot(query, "snap1")
        
        call_args = mock_db_session.execute.call_args
        executed_sql = str(call_args[0][0])
        assert "FROM   users {SNAPSHOT = 'snap1'}   WHERE" in executed_sql

    def test_query_at_snapshot_case_insensitivity(self, git_for_data, mock_db_session):
        """Test injection with case insensitivity."""
        query = "select * from Users"
        git_for_data.query_at_snapshot(query, "snap1")
        
        call_args = mock_db_session.execute.call_args
        executed_sql = str(call_args[0][0])
        assert "from Users {SNAPSHOT = 'snap1'}" in executed_sql

    def test_query_at_snapshot_already_has_snapshot(self, git_for_data, mock_db_session):
        """Test that we don't double inject if snapshot is already present."""
        query = "SELECT * FROM users {SNAPSHOT = 'old_snap'}"
        git_for_data.query_at_snapshot(query, "new_snap")
        
        call_args = mock_db_session.execute.call_args
        executed_sql = str(call_args[0][0])
        # Should NOT inject new_snap if old_snap is there? 
        # Or implementation logic says: if "{SNAPSHOT" in match, return match
        assert "FROM users {SNAPSHOT = 'old_snap'}" in executed_sql
        assert "new_snap" not in executed_sql

    def test_query_at_snapshot_invalid_snapshot_name(self, git_for_data):
        """Test validation of snapshot name."""
        with pytest.raises(ValueError):
            git_for_data.query_at_snapshot("SELECT * FROM users", "invalid-name")
