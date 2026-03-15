"""Unit tests for TurnHooks._maybe_trigger_episodic.

Tests all branches:
- no_episodic flag → skip
- short session (< MIN_EVENTS) → write topic stub once
- stub already written → skip
- long session, threshold not met → skip
- long session, count threshold met → call request_session_summary
- long session, time threshold met → call request_session_summary
- no events in DB → skip summary
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from unittest.mock import MagicMock, patch, call
import pytest


def _make_session(
    session_id: str,
    event_count: int = 0,
    metadata: dict | None = None,
) -> MagicMock:
    row = MagicMock()
    row.session_id = session_id
    row.event_count = event_count
    row.session_metadata = metadata or {}
    return row


def _make_event(event_type: str, content: str) -> MagicMock:
    ev = MagicMock()
    ev.event_type = event_type
    ev.content = content
    return ev


def _make_hooks(session_row, events=None):
    """Build TurnHooks with a mocked DB that returns the given session row and events."""
    from core.agent.turn_hooks import TurnHooks

    db = MagicMock()
    # session query
    db.query.return_value.filter.return_value.first.return_value = session_row
    # events query (chained)
    event_query = MagicMock()
    event_query.filter.return_value.order_by.return_value.limit.return_value.all.return_value = (
        events or []
    )
    # Make second query() call return event_query
    db.query.side_effect = [
        db.query.return_value,  # first call: SessionModel
        event_query,            # second call: EventModel
    ]

    db_factory = MagicMock()
    db_factory.return_value.__enter__ = MagicMock(return_value=db)
    db_factory.return_value.__exit__ = MagicMock(return_value=False)

    hooks = TurnHooks.__new__(TurnHooks)
    hooks._db = db_factory
    return hooks, db


class TestMaybeTriggerEpisodicNoEpisodic:
    def test_no_episodic_flag_skips(self):
        """Session with no_episodic=True must not call store or request_session_summary."""
        row = _make_session("sess1", event_count=5, metadata={"no_episodic": True})
        hooks, _ = _make_hooks(row)
        svc = MagicMock()

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.store.assert_not_called()
        svc.request_session_summary.assert_not_called()

    def test_session_not_found_skips(self):
        """Missing session row must not raise and must not call svc."""
        from core.agent.turn_hooks import TurnHooks

        db = MagicMock()
        db.query.return_value.filter.return_value.first.return_value = None
        db_factory = MagicMock()
        db_factory.return_value.__enter__ = MagicMock(return_value=db)
        db_factory.return_value.__exit__ = MagicMock(return_value=False)

        hooks = TurnHooks.__new__(TurnHooks)
        hooks._db = db_factory
        svc = MagicMock()

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.store.assert_not_called()


class TestMaybeTriggerEpisodicShortSession:
    def test_short_session_writes_stub(self):
        """event_count < MIN_EVENTS → write topic stub with EPISODIC type, T4, confidence=0.3."""
        from core.memory.types import MemoryType, TrustTier

        row = _make_session("sess1", event_count=3)
        hooks, db = _make_hooks(row)

        # _build_topic_stub queries EventModel
        user_event = _make_event("user_query", "How does episodic memory work?")
        db.query.side_effect = [
            db.query.return_value,  # SessionModel
            MagicMock(  # EventModel for stub (user_query)
                **{
                    "filter.return_value.order_by.return_value.first.return_value": user_event
                }
            ),
        ]

        svc = MagicMock()
        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.store.assert_called_once()
        call_kwargs = svc.store.call_args.kwargs
        assert call_kwargs["memory_type"] == MemoryType.EPISODIC
        assert call_kwargs["trust_tier"] == TrustTier.T4
        assert call_kwargs["initial_confidence"] == 0.3
        assert "Topic:" in call_kwargs["content"]
        assert call_kwargs["session_id"] == "sess1"

    def test_short_session_stub_already_written_skips(self):
        """If episodic_stub_written=True, do not write again."""
        row = _make_session("sess1", event_count=3, metadata={"episodic_stub_written": True})
        hooks, _ = _make_hooks(row)
        svc = MagicMock()

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.store.assert_not_called()

    def test_short_session_no_events_skips(self):
        """Short session with no events → no stub written."""
        row = _make_session("sess1", event_count=3)
        hooks, db = _make_hooks(row)

        # _build_topic_stub queries EventModel twice (user_query, then llm_response)
        no_event_query = MagicMock(
            **{"filter.return_value.order_by.return_value.first.return_value": None}
        )
        db.query.side_effect = [
            db.query.return_value,  # SessionModel
            no_event_query,         # EventModel user_query → None
            no_event_query,         # EventModel llm_response → None
        ]

        svc = MagicMock()
        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.store.assert_not_called()


class TestMaybeTriggerEpisodicLongSession:
    def test_count_threshold_triggers_summary(self):
        """event_count - last_count >= THRESHOLD → call request_session_summary."""
        from core.agent.turn_hooks import _EPISODIC_EVENT_THRESHOLD, _EPISODIC_MIN_EVENTS

        last_count = 10
        event_count = last_count + _EPISODIC_EVENT_THRESHOLD
        row = _make_session(
            "sess1",
            event_count=event_count,
            metadata={"episodic_last_event_count": last_count},
        )
        events = [
            _make_event("user_query", "Tell me about Memoria"),
            _make_event("llm_response", "Memoria is a memory system"),
        ]
        hooks, _ = _make_hooks(row, events=events)
        svc = MagicMock()
        svc.request_session_summary.return_value = {"task_id": "task_abc"}

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.request_session_summary.assert_called_once()
        call_kwargs = svc.request_session_summary.call_args.kwargs
        assert call_kwargs["user_id"] == "user1"
        assert call_kwargs["session_id"] == "sess1"
        assert call_kwargs["mode"] == "full"
        assert call_kwargs["sync"] is False
        assert len(call_kwargs["messages"]) == 2

    def test_time_threshold_triggers_summary(self):
        """Time since last summary >= TIME_THRESHOLD → call request_session_summary."""
        from core.agent.turn_hooks import _EPISODIC_TIME_THRESHOLD_SEC, _EPISODIC_MIN_EVENTS

        old_time = (
            datetime.now(timezone.utc) - timedelta(seconds=_EPISODIC_TIME_THRESHOLD_SEC + 60)
        ).isoformat()
        row = _make_session(
            "sess1",
            event_count=_EPISODIC_MIN_EVENTS + 5,
            metadata={
                "episodic_last_event_count": _EPISODIC_MIN_EVENTS + 4,  # count delta = 1 (below threshold)
                "episodic_last_at": old_time,
            },
        )
        events = [_make_event("user_query", "What is episodic memory?")]
        hooks, _ = _make_hooks(row, events=events)
        svc = MagicMock()
        svc.request_session_summary.return_value = {}

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.request_session_summary.assert_called_once()

    def test_neither_threshold_met_skips(self):
        """Neither count nor time threshold met → no summary."""
        from core.agent.turn_hooks import _EPISODIC_EVENT_THRESHOLD, _EPISODIC_MIN_EVENTS

        recent_time = datetime.now(timezone.utc).isoformat()
        last_count = _EPISODIC_MIN_EVENTS + 5
        event_count = last_count + 1  # delta = 1, below threshold
        row = _make_session(
            "sess1",
            event_count=event_count,
            metadata={
                "episodic_last_event_count": last_count,
                "episodic_last_at": recent_time,
            },
        )
        hooks, _ = _make_hooks(row)
        svc = MagicMock()

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.request_session_summary.assert_not_called()
        svc.store.assert_not_called()

    def test_no_events_in_db_skips_summary(self):
        """Long session but no events in DB → skip summary call."""
        from core.agent.turn_hooks import _EPISODIC_EVENT_THRESHOLD, _EPISODIC_MIN_EVENTS

        last_count = 10
        event_count = last_count + _EPISODIC_EVENT_THRESHOLD
        row = _make_session(
            "sess1",
            event_count=event_count,
            metadata={"episodic_last_event_count": last_count},
        )
        hooks, _ = _make_hooks(row, events=[])  # empty events
        svc = MagicMock()

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.request_session_summary.assert_not_called()

    def test_task_id_stored_in_metadata(self):
        """task_id from async summary must be persisted via DB update."""
        from core.agent.turn_hooks import _EPISODIC_EVENT_THRESHOLD

        last_count = 10
        event_count = last_count + _EPISODIC_EVENT_THRESHOLD
        row = _make_session(
            "sess1",
            event_count=event_count,
            metadata={"episodic_last_event_count": last_count},
        )
        events = [_make_event("user_query", "test")]
        hooks, db = _make_hooks(row, events=events)
        svc = MagicMock()
        svc.request_session_summary.return_value = {"task_id": "task_xyz"}

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        svc.request_session_summary.assert_called_once()
        # Phase 3 opens a new connection — verify db_factory was called twice
        assert hooks._db.call_count >= 2


class TestTrimTopic:
    def test_trim_adds_prefix(self):
        from core.agent.turn_hooks import TurnHooks
        assert TurnHooks._trim_topic("hello world") == "Topic: hello world"

    def test_trim_truncates_long_text(self):
        from core.agent.turn_hooks import TurnHooks, _EPISODIC_STUB_MAX_LEN
        long = "x" * (_EPISODIC_STUB_MAX_LEN + 50)
        result = TurnHooks._trim_topic(long)
        assert len(result) <= len("Topic: ") + _EPISODIC_STUB_MAX_LEN

    def test_trim_empty_returns_empty(self):
        from core.agent.turn_hooks import TurnHooks
        assert TurnHooks._trim_topic("") == ""
        assert TurnHooks._trim_topic("   ") == ""


class TestMaybeTriggerEpisodicErrorHandling:
    """Regression: DB commit failures must not propagate as unhandled exceptions."""

    def test_stub_commit_failure_does_not_raise(self):
        """Phase 3 commit failure must be caught and logged, not raised."""
        from sqlalchemy.exc import ProgrammingError

        row = _make_session("sess1", event_count=3)
        hooks, db = _make_hooks(row)

        user_event = _make_event("user_query", "test query long enough")
        db.query.side_effect = [
            db.query.return_value,
            MagicMock(**{
                "filter.return_value.order_by.return_value.first.return_value": user_event
            }),
        ]
        # Phase 3 opens a new db context — make it raise on commit
        write_db = MagicMock()
        write_db.__enter__ = MagicMock(return_value=write_db)
        write_db.__exit__ = MagicMock(return_value=False)
        write_db.commit.side_effect = ProgrammingError("no such table", {}, None)

        read_ctx = MagicMock()
        read_ctx.__enter__ = MagicMock(return_value=db)
        read_ctx.__exit__ = MagicMock(return_value=False)
        write_ctx = MagicMock()
        write_ctx.__enter__ = MagicMock(return_value=write_db)
        write_ctx.__exit__ = MagicMock(return_value=False)
        hooks._db.side_effect = [read_ctx, write_ctx]

        svc = MagicMock()
        # Must not raise
        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

    def test_summary_commit_failure_does_not_raise(self):
        """Phase 3 commit failure in summary path must not raise."""
        from sqlalchemy.exc import ProgrammingError
        from core.agent.turn_hooks import _EPISODIC_EVENT_THRESHOLD

        last_count = 10
        row = _make_session(
            "sess1",
            event_count=last_count + _EPISODIC_EVENT_THRESHOLD,
            metadata={"episodic_last_event_count": last_count},
        )
        events = [_make_event("user_query", "test")]
        hooks, db = _make_hooks(row, events=events)

        write_db = MagicMock()
        write_db.__enter__ = MagicMock(return_value=write_db)
        write_db.__exit__ = MagicMock(return_value=False)
        write_db.commit.side_effect = ProgrammingError("no such table", {}, None)

        read_ctx = MagicMock()
        read_ctx.__enter__ = MagicMock(return_value=db)
        read_ctx.__exit__ = MagicMock(return_value=False)
        write_ctx = MagicMock()
        write_ctx.__enter__ = MagicMock(return_value=write_db)
        write_ctx.__exit__ = MagicMock(return_value=False)
        hooks._db.side_effect = [read_ctx, write_ctx]

        svc = MagicMock()
        svc.request_session_summary.return_value = {}

        # Must not raise
        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

    def test_request_session_summary_failure_does_not_raise(self):
        """request_session_summary HTTP error must be caught and logged."""
        from core.agent.turn_hooks import _EPISODIC_EVENT_THRESHOLD

        last_count = 10
        row = _make_session(
            "sess1",
            event_count=last_count + _EPISODIC_EVENT_THRESHOLD,
            metadata={"episodic_last_event_count": last_count},
        )
        events = [_make_event("user_query", "test")]
        hooks, db = _make_hooks(row, events=events)

        svc = MagicMock()
        svc.request_session_summary.side_effect = Exception("connection refused")

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        # commit must NOT be called — we returned early after the exception
        db.commit.assert_not_called()

    def test_stub_store_failure_does_not_commit(self):
        """If svc.store() fails, metadata must not be committed."""
        row = _make_session("sess1", event_count=3)
        hooks, db = _make_hooks(row)

        user_event = _make_event("user_query", "test")
        db.query.side_effect = [
            db.query.return_value,
            MagicMock(**{
                "filter.return_value.order_by.return_value.first.return_value": user_event
            }),
        ]

        svc = MagicMock()
        svc.store.side_effect = Exception("Memoria unavailable")

        hooks._maybe_trigger_episodic(svc, "sess1", "user1")

        db.commit.assert_not_called()
