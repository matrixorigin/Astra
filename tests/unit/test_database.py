"""Tests for database connection and operations."""

import os
from unittest.mock import MagicMock, patch

import pymysql
import pytest

from db.database import Database, DatabaseConfig, get_db


class TestDatabaseConfig:
    """Test database configuration."""

    def test_config_defaults(self):
        """Test config loads defaults."""
        config = DatabaseConfig()

        assert config.host == os.getenv("DATABASE_HOST", "localhost")
        assert config.port == int(os.getenv("DATABASE_PORT", "6001"))
        assert config.user == os.getenv("DATABASE_USER", "dump")
        assert config.tenant == os.getenv("DATABASE_TENANT", "agent_platform")
        assert config.database == os.getenv("DATABASE_NAME", "agent_engine")

    def test_full_database_name(self):
        """Test full database name with tenant prefix."""
        config = DatabaseConfig()
        config.tenant = "test_tenant"
        config.database = "test_db"

        assert config.full_database_name == "test_tenant.test_db"


class TestDatabaseConnection:
    """Test database connection management."""

    @patch("db.database.pymysql.connect")
    def test_connect_success(self, mock_connect):
        """Test successful database connection."""
        mock_conn = MagicMock()
        mock_connect.return_value = mock_conn

        db = Database()
        conn = db.connect()

        assert conn == mock_conn
        mock_connect.assert_called_once()

    @patch("db.database.pymysql.connect")
    def test_connect_failure(self, mock_connect):
        """Test database connection failure."""
        mock_connect.side_effect = pymysql.Error("Connection failed")

        db = Database()
        with pytest.raises(pymysql.Error):
            db.connect()

    @patch("db.database.pymysql.connect")
    def test_close_connection(self, mock_connect):
        """Test closing database connection."""
        mock_conn = MagicMock()
        mock_connect.return_value = mock_conn

        db = Database()
        db.connect()
        db.close()

        mock_conn.close.assert_called_once()

    @patch("db.database.pymysql.connect")
    def test_get_connection_context_manager(self, mock_connect):
        """Test get_connection as context manager."""
        mock_conn = MagicMock()
        mock_connect.return_value = mock_conn

        db = Database()
        with db.get_connection() as conn:
            assert conn == mock_conn

        mock_conn.close.assert_called_once()


class TestDatabaseOperations:
    """Test database operations."""

    @patch("db.database.pymysql.connect")
    def test_execute_success(self, mock_connect):
        """Test successful query execution."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.rowcount = 1
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        rowcount = db.execute("INSERT INTO users VALUES (%s)", ("test",))

        assert rowcount == 1
        mock_cursor.execute.assert_called_once()
        mock_conn.commit.assert_called_once()

    @patch("db.database.pymysql.connect")
    def test_execute_no_commit(self, mock_connect):
        """Test query execution without commit."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.rowcount = 1
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        rowcount = db.execute("SELECT * FROM users", commit=False)

        assert rowcount == 1
        mock_conn.commit.assert_not_called()

    @patch("db.database.pymysql.connect")
    def test_execute_failure_rollback(self, mock_connect):
        """Test query execution failure triggers rollback."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.execute.side_effect = pymysql.Error("Query failed")
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        with pytest.raises(pymysql.Error):
            db.execute("INVALID SQL")

        mock_conn.rollback.assert_called_once()

    @patch("db.database.pymysql.connect")
    def test_fetchone_success(self, mock_connect):
        """Test fetchone returns single row."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.fetchone.return_value = {"id": 1, "name": "test"}
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        result = db.fetchone("SELECT * FROM users WHERE id = %s", (1,))

        assert result == {"id": 1, "name": "test"}
        mock_cursor.execute.assert_called_once()

    @patch("db.database.pymysql.connect")
    def test_fetchone_no_results(self, mock_connect):
        """Test fetchone returns None when no results."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.fetchone.return_value = None
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        result = db.fetchone("SELECT * FROM users WHERE id = %s", (999,))

        assert result is None

    @patch("db.database.pymysql.connect")
    def test_fetchone_failure(self, mock_connect):
        """Test fetchone handles query failure."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.execute.side_effect = pymysql.Error("Query failed")
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        with pytest.raises(pymysql.Error):
            db.fetchone("INVALID SQL")

    @patch("db.database.pymysql.connect")
    def test_fetchall_success(self, mock_connect):
        """Test fetchall returns multiple rows."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.fetchall.return_value = [
            {"id": 1, "name": "test1"},
            {"id": 2, "name": "test2"},
        ]
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        results = db.fetchall("SELECT * FROM users")

        assert len(results) == 2
        assert results[0]["id"] == 1
        assert results[1]["id"] == 2

    @patch("db.database.pymysql.connect")
    def test_fetchall_empty(self, mock_connect):
        """Test fetchall returns empty list when no results."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.fetchall.return_value = []
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        results = db.fetchall("SELECT * FROM users WHERE id = %s", (999,))

        assert results == []

    @patch("db.database.pymysql.connect")
    def test_fetchall_failure(self, mock_connect):
        """Test fetchall handles query failure."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.execute.side_effect = pymysql.Error("Query failed")
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        with pytest.raises(pymysql.Error):
            db.fetchall("INVALID SQL")


class TestDatabaseHealthCheck:
    """Test database health check."""

    @patch("db.database.pymysql.connect")
    def test_health_check_success(self, mock_connect):
        """Test health check returns True when database is healthy."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.fetchone.return_value = {"health": 1}
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        assert db.health_check() is True

    @patch("db.database.pymysql.connect")
    def test_health_check_failure(self, mock_connect):
        """Test health check returns False when database is unhealthy."""
        mock_connect.side_effect = pymysql.Error("Connection failed")

        db = Database()
        assert db.health_check() is False

    @patch("db.database.pymysql.connect")
    def test_health_check_invalid_response(self, mock_connect):
        """Test health check returns False for invalid response."""
        mock_conn = MagicMock()
        mock_cursor = MagicMock()
        mock_cursor.fetchone.return_value = None
        mock_conn.cursor.return_value = mock_cursor
        mock_connect.return_value = mock_conn

        db = Database()
        assert db.health_check() is False


class TestGetDbSingleton:
    """Test get_db singleton."""

    def test_get_db_returns_instance(self):
        """Test get_db returns Database instance."""
        db = get_db()
        assert isinstance(db, Database)

    def test_get_db_returns_same_instance(self):
        """Test get_db returns same instance (singleton)."""
        db1 = get_db()
        db2 = get_db()
        assert db1 is db2
