"""E2E tests for LLM call tracking: task_hint in metadata, auxiliary call stats.

Verifies that:
1. LLMCallLog.call_metadata contains task_hint when provided
2. track_auxiliary_calls() collects stats correctly
3. All fields in LLMCallLog are populated correctly
"""

import json
from contextlib import contextmanager
from datetime import datetime, timezone

import pytest
from uuid_utils import uuid7

from api.models.evaluation import LLMCallLog


class TestLogCallPersistence:
    """Verify _log_call writes task_hint into call_metadata column."""

    def test_task_hint_persisted_in_call_metadata(self, db_session):
        """_log_call with metadata={task_hint: X} → DB call_metadata contains it."""
        from core.llm.client import LLMClient
        from core.llm.models import LLMResponse

        client = LLMClient.__new__(LLMClient)

        # Wire _db to yield our test session
        @contextmanager
        def _db():
            yield db_session
        client._db = _db

        event_id = str(uuid7())
        response = LLMResponse(
            content="test", model="test-model", provider="openai",
            tokens_prompt=100, tokens_completion=50, tokens_total=150,
            latency_ms=200, cost_usd=0.003,
        )

        client._log_call(
            event_id, "test_user", "openai", response, "success",
            metadata={"task_hint": "memory_extraction", "extra": "data"},
        )

        # Re-query from DB — don't trust the ORM object
        log = db_session.query(LLMCallLog).filter_by(event_id=event_id).first()
        assert log is not None

        # Verify ALL fields
        assert log.user_id == "test_user"
        assert log.provider == "openai"
        assert log.model == "test-model"
        assert log.tokens_prompt == 100
        assert log.tokens_completion == 50
        assert log.tokens_total == 150
        assert log.cost_usd == pytest.approx(0.003)
        assert log.latency_ms == 200
        assert log.status == "success"
        assert log.error_message is None
        assert log.created_at is not None

        # The key assertion: call_metadata contains task_hint
        meta = json.loads(log.call_metadata)
        assert meta["task_hint"] == "memory_extraction"
        assert meta["extra"] == "data"

    def test_no_metadata_writes_null(self, db_session):
        """_log_call without metadata → call_metadata is NULL."""
        from core.llm.client import LLMClient
        from core.llm.models import LLMResponse

        client = LLMClient.__new__(LLMClient)

        @contextmanager
        def _db():
            yield db_session
        client._db = _db

        event_id = str(uuid7())
        response = LLMResponse(
            content="test", model="m", provider="openai",
            tokens_prompt=10, tokens_completion=5, tokens_total=15,
            latency_ms=50, cost_usd=0.0001,
        )

        client._log_call(event_id, "u", "openai", response, "success")

        log = db_session.query(LLMCallLog).filter_by(event_id=event_id).first()
        assert log is not None
        assert log.call_metadata is None

    def test_failed_call_persists_metadata(self, db_session):
        """_log_call for failed call → error_message + call_metadata both set."""
        from core.llm.client import LLMClient

        client = LLMClient.__new__(LLMClient)

        @contextmanager
        def _db():
            yield db_session
        client._db = _db

        event_id = str(uuid7())
        client._log_call(
            event_id, "u", "openai", None, "failed",
            error_message="timeout", latency_ms=5000,
            metadata={"task_hint": "cot_audit"},
        )

        log = db_session.query(LLMCallLog).filter_by(event_id=event_id).first()
        assert log is not None
        assert log.status == "failed"
        assert log.error_message == "timeout"
        assert log.latency_ms == 5000
        assert log.model == "unknown"
        assert log.tokens_total == 0

        meta = json.loads(log.call_metadata)
        assert meta["task_hint"] == "cot_audit"


class TestAuxiliaryCallTracker:
    """Verify track_auxiliary_calls() collects stats."""

    def test_tracker_collects_calls(self):
        """track_auxiliary_calls context manager accumulates call records."""
        from core.llm.client import LLMClient

        client = LLMClient.__new__(LLMClient)

        with client.track_auxiliary_calls() as calls:
            client._record_auxiliary("memory_extraction", 100, 50, 0.002, 200)
            client._record_auxiliary("cot_audit", 30, 10, 0.0005, 80)

        assert len(calls) == 2

        # Verify all fields of first call
        assert calls[0]["purpose"] == "memory_extraction"
        assert calls[0]["tokens_in"] == 100
        assert calls[0]["tokens_out"] == 50
        assert calls[0]["cost_usd"] == 0.002
        assert calls[0]["ms"] == 200

        # Second call
        assert calls[1]["purpose"] == "cot_audit"
        assert calls[1]["tokens_in"] == 30

    def test_tracker_noop_outside_context(self):
        """_record_auxiliary is a no-op when not inside track_auxiliary_calls."""
        from core.llm.client import LLMClient

        client = LLMClient.__new__(LLMClient)
        # Should not raise
        client._record_auxiliary("test", 10, 5, 0.001, 50)

    def test_tracker_isolation_between_contexts(self):
        """Each track_auxiliary_calls context gets its own list."""
        from core.llm.client import LLMClient

        client = LLMClient.__new__(LLMClient)

        with client.track_auxiliary_calls() as calls1:
            client._record_auxiliary("first", 10, 5, 0.001, 50)

        with client.track_auxiliary_calls() as calls2:
            client._record_auxiliary("second", 20, 10, 0.002, 100)

        assert len(calls1) == 1
        assert calls1[0]["purpose"] == "first"
        assert len(calls2) == 1
        assert calls2[0]["purpose"] == "second"
