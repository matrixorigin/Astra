"""Integration tests for A5: Replay migration.

Tests the two-path replay:
1. Chunk-level from run_events (primary)
2. Full-text fallback from conversation_events (when chunks missing)
"""

import json

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from core.agent.stream_replay import StreamReplay
from core.events.models import StreamEventType


def _uid():
    return str(uuid7())


@pytest.fixture
def run_id():
    return _uid()


@pytest.fixture
def session_id():
    return _uid()


@pytest.fixture
def chain_id():
    return _uid()


@pytest.fixture
def cleanup(db_session, run_id, session_id):
    yield
    try:
        db_session.execute(text("DELETE FROM run_events WHERE run_id = :r"), {"r": run_id})
        db_session.execute(
            text("DELETE FROM conversation_events WHERE session_id = :s"), {"s": session_id}
        )
        db_session.commit()
    except Exception:
        db_session.rollback()


def _insert_run_event(db, run_id, idx, event_type, data, event_id=None, agent_id=None):
    db.execute(
        text(
            "INSERT INTO run_events (run_id, idx, event_type, data, event_id, agent_id) "
            "VALUES (:run_id, :idx, :et, :data, :eid, :aid)"
        ),
        {
            "run_id": run_id,
            "idx": idx,
            "et": event_type,
            "data": json.dumps(data),
            "eid": event_id or _uid(),
            "aid": agent_id or "test-agent",
        },
    )


def _insert_llm_response(db, session_id, run_id, chain_id, content):
    eid = _uid()
    db.execute(
        text(
            "INSERT INTO conversation_events "
            "(event_id, session_id, user_id, agent_id, agent_version, "
            "event_type, content, causal_chain_id, run_id, created_at) "
            "VALUES (:eid, :sid, 'test_user', 'test-agent', '0.1', "
            "'llm_response', :content, :chain, :run_id, NOW())"
        ),
        {"eid": eid, "sid": session_id, "content": content, "chain": chain_id, "run_id": run_id},
    )
    return eid


class TestChunkLevelReplay:
    """Replay from run_events when run is complete."""

    @pytest.mark.asyncio
    async def test_completed_run_replays_chunks(self, db_session, run_id, session_id, chain_id, cleanup):
        """Normal completed run → chunk-level output from run_events."""
        # Populate run_events with stream chunks
        _insert_run_event(db_session, run_id, 0, "text_message_start", {"role": "assistant"})
        _insert_run_event(db_session, run_id, 1, "text_message_content", {"delta": "Hello "})
        _insert_run_event(db_session, run_id, 2, "text_message_content", {"delta": "world"})
        _insert_run_event(db_session, run_id, 3, "text_message_end", {})
        _insert_run_event(db_session, run_id, 4, "run_completed", {"status": "done"})
        db_session.commit()

        replay = StreamReplay(db_session)
        events = []
        async for ev in replay.replay_stream(session_id, run_id=run_id):
            events.append(ev)

        assert len(events) == 5
        assert events[0].event_type == StreamEventType.TEXT_MESSAGE_START
        assert events[1].event_type == StreamEventType.TEXT_MESSAGE_CONTENT
        assert events[1].data["delta"] == "Hello "
        assert events[2].data["delta"] == "world"
        assert events[3].event_type == StreamEventType.TEXT_MESSAGE_END

    @pytest.mark.asyncio
    async def test_chunk_output_matches_original_order(self, db_session, run_id, session_id, chain_id, cleanup):
        """Chunks replayed in idx order, matching original stream."""
        _insert_run_event(db_session, run_id, 0, "text_message_start", {"role": "assistant"})
        _insert_run_event(db_session, run_id, 1, "text_message_content", {"delta": "A"})
        _insert_run_event(db_session, run_id, 2, "text_message_content", {"delta": "B"})
        _insert_run_event(db_session, run_id, 3, "text_message_content", {"delta": "C"})
        _insert_run_event(db_session, run_id, 4, "text_message_end", {})
        _insert_run_event(db_session, run_id, 5, "run_completed", {})
        db_session.commit()

        replay = StreamReplay(db_session)
        deltas = []
        async for ev in replay.replay_stream(session_id, run_id=run_id):
            if ev.event_type == StreamEventType.TEXT_MESSAGE_CONTENT:
                deltas.append(ev.data["delta"])

        assert deltas == ["A", "B", "C"]


class TestFulltextFallback:
    """Fallback to llm_response when chunks are missing."""

    @pytest.mark.asyncio
    async def test_missing_chunks_falls_back_to_fulltext(self, db_session, run_id, session_id, chain_id, cleanup):
        """Simulated crash: no run_events → fallback to llm_response."""
        # Only llm_response in conversation_events, no run_events
        _insert_llm_response(db_session, session_id, run_id, chain_id, "Full response text")
        db_session.commit()

        replay = StreamReplay(db_session)
        events = []
        async for ev in replay.replay_stream(session_id, run_id=run_id):
            events.append(ev)

        # Should synthesize start → content → end
        assert len(events) == 3
        assert events[0].event_type == StreamEventType.TEXT_MESSAGE_START
        assert events[1].event_type == StreamEventType.TEXT_MESSAGE_CONTENT
        assert events[1].data["delta"] == "Full response text"
        assert events[2].event_type == StreamEventType.TEXT_MESSAGE_END

    @pytest.mark.asyncio
    async def test_incomplete_run_falls_back(self, db_session, run_id, session_id, chain_id, cleanup):
        """Run with chunks but no terminal event → falls back to full-text."""
        # Chunks exist but no run_completed/run_failed
        _insert_run_event(db_session, run_id, 0, "text_message_start", {"role": "assistant"})
        _insert_run_event(db_session, run_id, 1, "text_message_content", {"delta": "partial"})
        # No terminal event — run not complete

        _insert_llm_response(db_session, session_id, run_id, chain_id, "Complete response")
        db_session.commit()

        replay = StreamReplay(db_session)
        events = []
        async for ev in replay.replay_stream(session_id, run_id=run_id):
            events.append(ev)

        # Should use full-text, not partial chunks
        assert len(events) == 3
        assert events[1].data["delta"] == "Complete response"


class TestToolOnlyTurn:
    """Tool-only turns produce zero stream events — no crash."""

    @pytest.mark.asyncio
    async def test_tool_only_no_crash(self, db_session, run_id, session_id, chain_id, cleanup):
        """Run with only tool events, no llm_response → empty replay, no error."""
        # Insert tool_call event (not llm_response)
        eid = _uid()
        db_session.execute(
            text(
                "INSERT INTO conversation_events "
                "(event_id, session_id, user_id, agent_id, agent_version, "
                "event_type, content, causal_chain_id, run_id, created_at) "
                "VALUES (:eid, :sid, 'test_user', 'test-agent', '0.1', "
                "'tool_call', :content, :chain, :run_id, NOW())"
            ),
            {"eid": eid, "sid": session_id, "content": "calling tool X", "chain": chain_id, "run_id": run_id},
        )
        db_session.commit()

        replay = StreamReplay(db_session)
        events = []
        async for ev in replay.replay_stream(session_id, run_id=run_id):
            events.append(ev)

        assert events == []  # No crash, empty result


class TestCrossWorkerReplay:
    """Cross-worker: chunk replay gated on run completion."""

    @pytest.mark.asyncio
    async def test_waits_for_completion(self, db_session, run_id, session_id, chain_id, cleanup):
        """Without terminal event, chunk path is skipped (cross-worker safety)."""
        _insert_run_event(db_session, run_id, 0, "text_message_content", {"delta": "chunk"})
        db_session.commit()

        replay = StreamReplay(db_session)

        # No terminal event → _is_run_complete returns False → chunks skipped
        assert not replay._is_run_complete(run_id)

        # After adding terminal event → complete
        _insert_run_event(db_session, run_id, 1, "run_completed", {})
        db_session.commit()

        assert replay._is_run_complete(run_id)

        events = []
        async for ev in replay.replay_stream(session_id, run_id=run_id):
            events.append(ev)

        assert len(events) == 2  # chunk + run_completed

    @pytest.mark.asyncio
    async def test_failed_run_also_replays(self, db_session, run_id, session_id, chain_id, cleanup):
        """Failed runs are also considered complete for replay."""
        _insert_run_event(db_session, run_id, 0, "text_message_content", {"delta": "partial"})
        _insert_run_event(db_session, run_id, 1, "run_failed", {"error": "timeout"})
        db_session.commit()

        replay = StreamReplay(db_session)
        assert replay._is_run_complete(run_id)

        events = []
        async for ev in replay.replay_stream(session_id, run_id=run_id):
            events.append(ev)

        assert len(events) == 2


class TestLegacyPathPreserved:
    """Legacy path (no run_id) still works unchanged."""

    @pytest.mark.asyncio
    async def test_no_run_id_uses_legacy(self, db_session, session_id, chain_id, cleanup, run_id):
        """Without run_id, replay uses stream_* events from conversation_events."""
        eid = _uid()
        db_session.execute(
            text(
                "INSERT INTO conversation_events "
                "(event_id, session_id, user_id, agent_id, agent_version, "
                "event_type, content, causal_chain_id, created_at) "
                "VALUES (:eid, :sid, 'test_user', 'test-agent', '0.1', "
                "'stream_text_delta', :content, :chain, NOW())"
            ),
            {
                "eid": eid,
                "sid": session_id,
                "content": json.dumps({"event_type": "text_delta", "data": {"delta": "hi"}, "stream_event_id": eid}),
                "chain": chain_id,
            },
        )
        db_session.commit()

        replay = StreamReplay(db_session)
        events = []
        async for ev in replay.replay_stream(session_id):
            events.append(ev)

        assert len(events) == 1
        assert events[0].event_type == StreamEventType.TEXT_DELTA
