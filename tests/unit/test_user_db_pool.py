"""Tests for UserDBPool — per-user BYOD connection pool."""

import threading
from types import SimpleNamespace
from unittest.mock import MagicMock, patch

import pytest

from core.skills.user_db_pool import UserDBPool


def _fake_conn(dialect="mysql", host="localhost", port=3306, database="testdb",
               username="user", password_decrypted="pass"):
    return SimpleNamespace(
        dialect=dialect, host=host, port=port, database=database,
        username=username, password_decrypted=password_decrypted,
    )


class TestUserDBPool:
    def test_build_url_basic(self):
        pool = UserDBPool()
        conn = _fake_conn()
        url = pool._build_url(conn)
        assert "mysql+pymysql://user:pass@localhost:3306/testdb" in url

    def test_build_url_matrixone_uses_mysql_dialect(self):
        """matrixone dialect should still produce mysql+pymysql URL."""
        pool = UserDBPool()
        conn = _fake_conn(dialect="matrixone")
        url = pool._build_url(conn)
        assert url.startswith("mysql+pymysql://")

    def test_build_url_encodes_special_chars_in_password(self):
        pool = UserDBPool()
        conn = _fake_conn(password_decrypted="p@ss:w/rd#123")
        url = pool._build_url(conn)
        assert "p@ss" not in url  # should be encoded
        assert "p%40ss%3Aw%2Frd%23123" in url

    def test_build_url_encodes_special_chars_in_username(self):
        pool = UserDBPool()
        conn = _fake_conn(username="user@domain")
        url = pool._build_url(conn)
        assert "user%40domain" in url

    @patch("core.skills.user_db_pool.create_engine")
    def test_get_engine_caches(self, mock_create):
        mock_engine = MagicMock()
        mock_create.return_value = mock_engine
        pool = UserDBPool()
        conn = _fake_conn()

        e1 = pool.get_engine("u1", conn)
        e2 = pool.get_engine("u1", conn)
        assert e1 is e2
        assert mock_create.call_count == 1

    @patch("core.skills.user_db_pool.create_engine")
    def test_different_users_different_engines(self, mock_create):
        mock_create.side_effect = [MagicMock(), MagicMock()]
        pool = UserDBPool()
        conn = _fake_conn()

        e1 = pool.get_engine("u1", conn)
        e2 = pool.get_engine("u2", conn)
        assert e1 is not e2
        assert mock_create.call_count == 2

    @patch("core.skills.user_db_pool.create_engine")
    def test_close_user_disposes_engine(self, mock_create):
        mock_engine = MagicMock()
        mock_create.return_value = mock_engine
        pool = UserDBPool()
        pool.get_engine("u1", _fake_conn())

        pool.close_user("u1")
        mock_engine.dispose.assert_called_once()
        # Second close is no-op
        pool.close_user("u1")

    @patch("core.skills.user_db_pool.create_engine")
    def test_close_all(self, mock_create):
        engines = [MagicMock(), MagicMock()]
        mock_create.side_effect = engines
        pool = UserDBPool()
        pool.get_engine("u1", _fake_conn())
        pool.get_engine("u2", _fake_conn())

        pool.close_all()
        for e in engines:
            e.dispose.assert_called_once()

    @patch("core.skills.user_db_pool.create_engine")
    def test_get_session_returns_session(self, mock_create):
        mock_engine = MagicMock()
        mock_create.return_value = mock_engine
        pool = UserDBPool()
        session = pool.get_session("u1", _fake_conn())
        assert session is not None

    @patch("core.skills.user_db_pool.create_engine")
    def test_thread_safety(self, mock_create):
        """Concurrent get_engine calls should not create duplicate engines."""
        real_engine = MagicMock()
        mock_create.return_value = real_engine
        pool = UserDBPool()
        conn = _fake_conn()
        results = []

        def worker():
            results.append(pool.get_engine("u1", conn))

        threads = [threading.Thread(target=worker) for _ in range(10)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert all(r is real_engine for r in results)
        assert mock_create.call_count == 1

    @patch("core.skills.user_db_pool.create_engine")
    def test_pool_config_passed(self, mock_create):
        mock_create.return_value = MagicMock()
        pool = UserDBPool(pool_size=5, max_overflow=3, pool_recycle=900)
        pool.get_engine("u1", _fake_conn())
        _, kwargs = mock_create.call_args
        assert kwargs["pool_size"] == 5
        assert kwargs["max_overflow"] == 3
        assert kwargs["pool_recycle"] == 900
        assert kwargs["pool_pre_ping"] is True
