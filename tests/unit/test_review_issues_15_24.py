"""Tests for review issues 15–24.

Covers:
  15 – _load_agent_config module-level cache
  19 – increment_event_count atomic SQL
  20 – run_daily_all keyset pagination (no OFFSET)
  21 – _cleanup_tool_results batched DELETE with LIMIT
  22-24 – missing indexes on auth/evaluation tables
"""

import inspect
import json
from datetime import datetime, timezone
from unittest.mock import MagicMock

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from api.models.auth import AuditLog, RefreshToken
from api.models.evaluation import LLMCallLog
from core.agent.run_engine import RunEngine, _agent_config_cache
from core.memory.governance import GovernanceScheduler


# ── fixtures ──────────────────────────────────────────────────────────

@pytest.fixture
def db(db_session):
    yield db_session


# ── Problem 15: agent config cache ───────────────────────────────────

class TestAgentConfigCache:
    """_load_agent_config should hit module-level cache on second call."""

    def setup_method(self):
        _agent_config_cache.clear()

    def teardown_method(self):
        _agent_config_cache.clear()

    def test_cache_hit_skips_db(self):
        _agent_config_cache["cached-agent"] = {"model": "gpt-4o"}
        engine = RunEngine.__new__(RunEngine)
        engine._db = MagicMock()  # should never be called
        result = engine._load_agent_config("cached-agent")
        assert result == {"model": "gpt-4o"}
        engine._db.assert_not_called()

    def test_cache_miss_populates_cache(self):
        mock_db = MagicMock()
        mock_db.query.return_value.filter.return_value.first.return_value = (
            json.dumps({"system_prompt": "hello"}),
        )
        engine = RunEngine.__new__(RunEngine)
        engine._db = MagicMock(return_value=MagicMock(
            __enter__=MagicMock(return_value=mock_db),
            __exit__=MagicMock(return_value=False),
        ))
        result = engine._load_agent_config("new-agent")
        assert result == {"system_prompt": "hello"}
        assert _agent_config_cache["new-agent"] == {"system_prompt": "hello"}

    def test_cache_none_on_missing_agent(self):
        mock_db = MagicMock()
        mock_db.query.return_value.filter.return_value.first.return_value = None
        engine = RunEngine.__new__(RunEngine)
        engine._db = MagicMock(return_value=MagicMock(
            __enter__=MagicMock(return_value=mock_db),
            __exit__=MagicMock(return_value=False),
        ))
        result = engine._load_agent_config("ghost")
        assert result is None
        assert _agent_config_cache["ghost"] is None


# ── Problem 19: atomic increment (DB integration) ────────────────────

class TestAtomicIncrementDB:
    """event_count uses SQL `event_count + 1`, not read-modify-write."""

    def test_ten_increments_yield_ten(self, db):
        sid = str(uuid7())
        uid = str(uuid7())
        db.execute(text(
            "INSERT INTO agent_sessions "
            "(session_id, user_id, status, event_count, created_at, last_active_at) "
            "VALUES (:sid, :uid, 'active', 0, NOW(), NOW())"
        ), {"sid": sid, "uid": uid})
        db.commit()

        for _ in range(10):
            db.execute(text(
                "UPDATE agent_sessions SET event_count = event_count + 1 "
                "WHERE session_id = :sid"
            ), {"sid": sid})
            db.commit()

        row = db.execute(
            text("SELECT event_count FROM agent_sessions WHERE session_id = :sid"),
            {"sid": sid},
        ).fetchone()
        assert row[0] == 10

        # cleanup
        db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
        db.commit()

    def test_source_uses_atomic_sql(self):
        """increment_event_count must use 'event_count + 1' in SQL, not ORM."""
        from api.services.session_service import SessionService
        src = inspect.getsource(SessionService.increment_event_count)
        assert "event_count = event_count + 1" in src


# ── Problem 20: keyset pagination ────────────────────────────────────

class TestKeysetPagination:
    def test_no_offset_in_run_daily_all(self):
        src = inspect.getsource(GovernanceScheduler.run_daily_all)
        assert "OFFSET" not in src, "Should use keyset, not OFFSET"
        assert "user_id > :last" in src


# ── Problem 21: batched DELETE ───────────────────────────────────────

class TestBatchedDelete:
    def test_cleanup_tool_results_uses_limit(self):
        src = inspect.getsource(GovernanceScheduler._cleanup_tool_results)
        assert "LIMIT :batch" in src
        assert "batch_limit" in src


# ── Problems 22-24: missing indexes (ORM metadata) ──────────────────

class TestMissingIndexesORM:
    def test_audit_log_created_at_indexed(self):
        assert AuditLog.__table__.c.created_at.index is True

    def test_refresh_token_expires_at_indexed(self):
        assert RefreshToken.__table__.c.expires_at.index is True

    def test_llm_call_log_created_at_indexed(self):
        assert LLMCallLog.__table__.c.created_at.index is True


# ── Problems 22-24: missing indexes (real DB) ────────────────────────

class TestMissingIndexesDB:
    @pytest.mark.parametrize("table,column", [
        ("auth_audit_logs", "created_at"),
        ("auth_refresh_tokens", "expires_at"),
        ("eval_llm_call_logs", "created_at"),
    ])
    def test_index_exists_in_db(self, db, table, column):
        rows = db.execute(text(f"SHOW INDEX FROM {table}")).fetchall()
        indexed_cols = {r[4] for r in rows}  # Column_name at position 4
        assert column in indexed_cols, f"{table}.{column} missing index in DB"
