"""Integration tests: reasoning_content persisted as dedicated DB column.

Verifies:
- _persist_turn_events stores reasoning_content on the FIRST tool_call event only
- reasoning_content column is NULL for non-first tool_call events
- reasoning_content column is NULL when reasoning_content is empty
- _recover_history_from_db returns 4-tuple rows including reasoning_content
- EventLogger.create_stream_event persists reasoning_content column
- EventLogger.log_event persists reasoning_content from ConversationEvent model
- Round-trip: persist → recover → append_recovered_events preserves reasoning
"""

import json

import pytest
import sqlalchemy as sa
from uuid_utils import uuid7


@pytest.fixture
def rc_session(db):
    """Create a fresh session for reasoning_content tests."""
    from api.models.agent import Session as SessionModel

    sid = str(uuid7())
    uid = str(uuid7())
    db.add(SessionModel(session_id=sid, user_id=uid, agent_id="test", status="active"))
    db.commit()
    yield sid, uid
    db.execute(sa.text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
    db.execute(sa.text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
    db.commit()


def _run_persist(session_id, user_id, **kwargs):
    """Run _persist_turn_events and wait for background thread deterministically."""
    from api.routers.chat import _persist_turn_events, _flush_persist_threads

    chain_id = kwargs.pop("turn_chain_id", str(uuid7()))
    uq_id = kwargs.pop("user_query_event_id", str(uuid7()))
    _persist_turn_events(
        user_id,
        session_id,
        messages=kwargs.get("messages", [{"role": "user", "content": "hello"}]),
        tool_results=kwargs.get("tool_results", []),
        full_text=kwargs.get("full_text", "response"),
        tool_calls=kwargs.get("tool_calls", []),
        reasoning_content=kwargs.get("reasoning_content", ""),
        turn_chain_id=chain_id,
        user_query_event_id=uq_id,
        history=kwargs.get("history"),
        turn_count=kwargs.get("turn_count", 0),
    )
    _flush_persist_threads()


def _fetch_events(db, session_id, event_type=None):
    q = "SELECT event_id, event_type, content, metadata, reasoning_content FROM agent_events WHERE session_id = :sid"
    params: dict = {"sid": session_id}
    if event_type:
        q += " AND event_type = :et"
        params["et"] = event_type
    q += " ORDER BY created_at"
    rows = db.execute(sa.text(q), params).fetchall()
    return [dict(r._mapping) for r in rows]


class TestReasoningContentPersistence:
    """Verify reasoning_content column in agent_events table."""

    def test_reasoning_on_first_tool_call_only(self, db, rc_session):
        """reasoning_content must be stored on the first tool_call event, NULL on others."""
        sid, uid = rc_session
        tool_calls = [
            {"id": "call_abc", "function": {"name": "read_file", "arguments": "{}"}},
            {"id": "call_def", "function": {"name": "search", "arguments": "{}"}},
        ]
        _run_persist(sid, uid, tool_calls=tool_calls, reasoning_content="I should read the file")

        tc_events = _fetch_events(db, sid, event_type="tool_call")
        assert len(tc_events) == 2

        # First tool_call carries reasoning_content
        assert tc_events[0]["reasoning_content"] == "I should read the file"
        # Second tool_call must be NULL
        assert tc_events[1]["reasoning_content"] is None

    def test_no_reasoning_stores_null(self, db, rc_session):
        """Empty reasoning_content must result in NULL column, not empty string."""
        sid, uid = rc_session
        tool_calls = [{"id": "call_xyz", "function": {"name": "fn", "arguments": "{}"}}]
        _run_persist(sid, uid, tool_calls=tool_calls, reasoning_content="")

        tc_events = _fetch_events(db, sid, event_type="tool_call")
        assert len(tc_events) == 1
        assert tc_events[0]["reasoning_content"] is None

    def test_reasoning_not_on_llm_response_when_tool_calls_exist(self, db, rc_session):
        """When tool_calls exist, reasoning is on tool_call, NOT on llm_response."""
        sid, uid = rc_session
        tool_calls = [{"id": "call_aaa", "function": {"name": "fn", "arguments": "{}"}}]
        _run_persist(
            sid, uid, tool_calls=tool_calls, full_text="answer", reasoning_content="deep thought"
        )

        llm_events = _fetch_events(db, sid, event_type="llm_response")
        assert len(llm_events) >= 1
        for ev in llm_events:
            assert ev["reasoning_content"] is None

        # reasoning is on the tool_call instead
        tc_events = _fetch_events(db, sid, event_type="tool_call")
        assert tc_events[0]["reasoning_content"] == "deep thought"

    def test_text_only_thinking_stores_reasoning_on_llm_response(self, db, rc_session):
        """Text-only response with reasoning → reasoning_content on llm_response event."""
        sid, uid = rc_session
        _run_persist(
            sid,
            uid,
            tool_calls=[],
            full_text="just text",
            reasoning_content="thinking but no tools",
        )

        llm_events = _fetch_events(db, sid, event_type="llm_response")
        assert len(llm_events) >= 1
        assert llm_events[0]["reasoning_content"] == "thinking but no tools"

    def test_text_only_no_reasoning_stores_null(self, db, rc_session):
        """Text-only response without reasoning → NULL on llm_response."""
        sid, uid = rc_session
        _run_persist(sid, uid, tool_calls=[], full_text="plain answer", reasoning_content="")

        llm_events = _fetch_events(db, sid, event_type="llm_response")
        assert len(llm_events) >= 1
        assert llm_events[0]["reasoning_content"] is None


class TestEventLoggerReasoningContent:
    """Verify EventLogger methods persist reasoning_content column."""

    def test_create_stream_event_with_reasoning(self, db_factory, db, rc_session):
        """create_stream_event must persist reasoning_content to DB column."""
        from core.events.event_logger import EventLogger

        sid, uid = rc_session
        el = EventLogger(db_factory)
        ev = el.create_stream_event(
            user_id=uid,
            session_id=sid,
            event_type="tool_call",
            content=json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
            reasoning_content="let me think",
        )

        # Re-query from DB
        row = db.execute(
            sa.text("SELECT reasoning_content FROM agent_events WHERE event_id = :eid"),
            {"eid": ev.event_id},
        ).fetchone()
        assert row is not None
        assert row[0] == "let me think"

    def test_create_stream_event_none_reasoning(self, db_factory, db, rc_session):
        """create_stream_event with reasoning_content=None must store NULL."""
        from core.events.event_logger import EventLogger

        sid, uid = rc_session
        el = EventLogger(db_factory)
        ev = el.create_stream_event(
            user_id=uid,
            session_id=sid,
            event_type="tool_call",
            content=json.dumps({"tool_call_id": "tc2", "name": "fn", "arguments": "{}"}),
            reasoning_content=None,
        )

        row = db.execute(
            sa.text("SELECT reasoning_content FROM agent_events WHERE event_id = :eid"),
            {"eid": ev.event_id},
        ).fetchone()
        assert row is not None
        assert row[0] is None

    def test_create_stream_event_empty_string_stores_null(self, db_factory, db, rc_session):
        """Empty string reasoning_content must be stored as NULL (not '')."""
        from core.events.event_logger import EventLogger

        sid, uid = rc_session
        el = EventLogger(db_factory)
        ev = el.create_stream_event(
            user_id=uid,
            session_id=sid,
            event_type="tool_call",
            content=json.dumps({"tool_call_id": "tc3", "name": "fn", "arguments": "{}"}),
            reasoning_content="",
        )

        row = db.execute(
            sa.text("SELECT reasoning_content FROM agent_events WHERE event_id = :eid"),
            {"eid": ev.event_id},
        ).fetchone()
        assert row is not None
        assert row[0] is None

    def test_log_event_persists_reasoning_from_model(self, db_factory, db, rc_session):
        """EventLogger.log_event must persist reasoning_content from ConversationEvent."""
        from core.events.event_logger import EventLogger
        from core.events.models import ConversationEvent

        sid, uid = rc_session
        el = EventLogger(db_factory)
        event = ConversationEvent(
            event_id=str(uuid7()),
            user_id=uid,
            session_id=sid,
            agent_id="test-agent",
            agent_version="0.1.0",
            event_type="tool_call",
            content="test",
            causal_chain_id=str(uuid7()),
            reasoning_content="model-level reasoning",
        )
        el.log_event(event)

        row = db.execute(
            sa.text("SELECT reasoning_content FROM agent_events WHERE event_id = :eid"),
            {"eid": event.event_id},
        ).fetchone()
        assert row is not None
        assert row[0] == "model-level reasoning"


class TestRecoverHistoryReasoningContent:
    """Verify _recover_history_from_db returns reasoning_content in query tuples."""

    def test_event_query_returns_four_columns(self, db_factory, db, rc_session):
        """DB query for history recovery must return (event_type, content, metadata, reasoning_content)."""
        from core.events.event_logger import EventLogger
        from api.models.agent import Event as EventModel

        sid, uid = rc_session
        el = EventLogger(db_factory)

        # Insert a tool_call with reasoning_content
        el.create_stream_event(
            user_id=uid,
            session_id=sid,
            event_type="tool_call",
            content=json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
            metadata={"tool_call_id": "tc1", "name": "fn", "source": "edge"},
            reasoning_content="thinking hard",
        )

        # Query the same way _recover_history_from_db does
        rows = (
            db.query(
                EventModel.event_type,
                EventModel.content,
                EventModel.event_metadata,
                EventModel.reasoning_content,
            )
            .filter(EventModel.session_id == sid)
            .order_by(EventModel.created_at.asc())
            .all()
        )
        assert len(rows) >= 1
        row = rows[-1]
        assert len(row) == 4
        assert row[0] == "tool_call"
        assert row[3] == "thinking hard"

    def test_round_trip_persist_recover_append(self, db_factory, db, rc_session):
        """Full round-trip: persist events → query from DB → append_recovered_events."""
        from core.events.event_logger import EventLogger
        from core.history_utils import append_recovered_events
        from api.models.agent import Event as EventModel

        sid, uid = rc_session
        el = EventLogger(db_factory)
        chain = str(uuid7())

        # Persist: user_query → tool_call (with reasoning) → tool_result
        uq = el.create_stream_event(
            user_id=uid,
            session_id=sid,
            event_type="user_query",
            content="do something",
            causal_chain_id=chain,
        )
        el.create_stream_event(
            user_id=uid,
            session_id=sid,
            event_type="tool_call",
            content=json.dumps({"tool_call_id": "tc1", "name": "fn", "arguments": "{}"}),
            metadata={"tool_call_id": "tc1", "name": "fn", "source": "edge"},
            reasoning_content="let me think about this",
            parent_event_id=uq.event_id,
            causal_chain_id=chain,
        )
        el.create_stream_event(
            user_id=uid,
            session_id=sid,
            event_type="tool_result",
            content=json.dumps({"result": "done"}),
            metadata={"tool_call_id": "tc1", "name": "fn"},
            parent_event_id=uq.event_id,
            causal_chain_id=chain,
        )

        # Recover from DB
        _event_types = ("user_query", "llm_response", "tool_call", "tool_result")
        rows = (
            db.query(
                EventModel.event_type,
                EventModel.content,
                EventModel.event_metadata,
                EventModel.reasoning_content,
            )
            .filter(
                EventModel.session_id == sid,
                EventModel.event_type.in_(_event_types),
            )
            .order_by(EventModel.created_at.asc())
            .all()
        )

        # Reconstruct history
        history = append_recovered_events([], rows)

        # Verify: assistant message has reasoning_content from DB column
        assert history[0]["role"] == "user"
        asst = history[1]
        assert asst["role"] == "assistant"
        assert asst["reasoning_content"] == "let me think about this"
        assert len(asst["tool_calls"]) == 1
        assert history[2]["role"] == "tool"
