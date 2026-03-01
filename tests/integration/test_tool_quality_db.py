"""DB integration test: tool_result_quality event write → reflect read.

Verifies the FULL data path:
  1. EventLogger.create_stream_event writes tool_result_quality with correct event_type and metadata
  2. _build_reflect_evidence queries it back and surfaces it in tool_quality_summary
  3. All DB fields (event_type, content, metadata keys) match between writer and reader

This catches field-name mismatches, missing enum values, and serialization bugs
that mock-based tests cannot detect.
"""

import json
import pytest

from api.database import SessionLocal
from core.events.session_manager import SessionManager
from core.events.event_logger import EventLogger
from sqlalchemy import text


@pytest.fixture
def quality_session():
    """Create a real DB session with a tool_result_quality event."""
    user_id = "quality_db_tst"
    mgr = SessionManager(SessionLocal())
    session = mgr.create_session(user_id=user_id)
    sid = session.session_id

    el = EventLogger(SessionLocal)
    uq = el.create_user_query(user_id=user_id, session_id=sid,
                               content="中信证券建议买吗？")
    chain = uq.causal_chain_id

    # Simulate tool_call + tool_result
    el.create_stream_event(
        user_id=user_id, session_id=sid,
        event_type="tool_call",
        content=json.dumps({"name": "stock_assistant", "tool_call_id": "tc_q1"}),
        parent_event_id=uq.event_id, causal_chain_id=chain,
        skill_name="stock_assistant",
    )
    el.create_stream_event(
        user_id=user_id, session_id=sid,
        event_type="tool_result",
        content=json.dumps({"name": "stock_assistant", "result": "{}"}),
        parent_event_id=uq.event_id, causal_chain_id=chain,
        skill_name="stock_assistant",
    )

    # Write the quality event — same code path as Phase 2b in _persist_turn_events
    el.create_stream_event(
        user_id=user_id, session_id=sid,
        event_type="tool_result_quality",
        content=json.dumps({
            "tool_name": "stock_assistant", "score": 0.3,
            "grade": "degraded",
            "signals": ["empty_containers: 5/7 fields empty", "zero_cluster: 3 numeric fields are 0"],
            "stale": False,
        }),
        parent_event_id=uq.event_id,
        causal_chain_id=chain,
        metadata={
            "tool_name": "stock_assistant",
            "quality_score": 0.3,
            "quality_grade": "degraded",
            "signals": ["empty_containers: 5/7 fields empty", "zero_cluster: 3 numeric fields are 0"],
            "stale": False,
        },
    )

    # Also add an LLM response so the session looks complete
    el.create_llm_response(
        user_id=user_id, session_id=sid,
        content="数据不完整，无法给出可靠建议。",
        agent_id="dev-agent", agent_version="0.1.0",
        parent_event_id=uq.event_id, causal_chain_id=chain,
    )

    yield sid, user_id, chain

    db = SessionLocal()
    for table in ("agent_events", "agent_sessions"):
        try:
            db.execute(text(f"DELETE FROM {table} WHERE session_id = :sid"), {"sid": sid})
        except Exception:
            pass
    db.commit()
    db.close()


class TestQualityEventDBRoundtrip:
    """Verify tool_result_quality events survive DB write → read."""

    def test_event_type_persisted_correctly(self, quality_session):
        """event_type must be 'tool_result_quality', not 'system_message'."""
        sid, user_id, _ = quality_session
        db = SessionLocal()
        try:
            from api.models.agent import Event as EventModel
            rows = (
                db.query(EventModel.event_type, EventModel.content, EventModel.event_metadata)
                .filter(
                    EventModel.session_id == sid,
                    EventModel.event_type == "tool_result_quality",
                )
                .all()
            )
            assert len(rows) == 1, (
                f"Expected 1 tool_result_quality event, got {len(rows)}. "
                f"Check EventType enum includes TOOL_RESULT_QUALITY."
            )
            event_type, content, metadata = rows[0]
            assert event_type == "tool_result_quality"
        finally:
            db.close()

    def test_metadata_fields_match_reflect_query(self, quality_session):
        """Metadata field names must match what _build_reflect_evidence reads."""
        sid, user_id, _ = quality_session
        db = SessionLocal()
        try:
            from api.models.agent import Event as EventModel
            row = (
                db.query(EventModel.event_metadata)
                .filter(
                    EventModel.session_id == sid,
                    EventModel.event_type == "tool_result_quality",
                )
                .first()
            )
            assert row is not None
            meta = row[0]
            if isinstance(meta, str):
                meta = json.loads(meta)

            # These are the exact field names _build_reflect_evidence reads
            assert "quality_grade" in meta, f"Missing 'quality_grade' in metadata: {meta}"
            assert "quality_score" in meta, f"Missing 'quality_score' in metadata: {meta}"
            assert "tool_name" in meta, f"Missing 'tool_name' in metadata: {meta}"

            assert meta["quality_grade"] == "degraded"
            assert meta["quality_score"] == 0.3
            assert meta["tool_name"] == "stock_assistant"
        finally:
            db.close()

    def test_reflect_surfaces_quality_event(self, quality_session):
        """_build_reflect_evidence must return the quality event in tool_quality_summary."""
        sid, user_id, _ = quality_session

        from api.routers.chat import _build_reflect_evidence
        evidence = _build_reflect_evidence(
            session_id=sid, user_id=user_id,
            focus="auto", last_n=50,
        )

        summary = evidence.get("tool_quality_summary", [])
        assert len(summary) >= 1, (
            f"Expected tool_quality_summary to contain degraded event, got: {summary}. "
            f"Full evidence keys: {list(evidence.keys())}"
        )
        item = summary[0]
        assert item["tool"] == "stock_assistant"
        assert item["grade"] == "degraded"
        assert item["score"] == 0.3

    def test_content_field_has_full_assessment(self, quality_session):
        """content field should contain the full JSON assessment for audit."""
        sid, _, _ = quality_session
        db = SessionLocal()
        try:
            from api.models.agent import Event as EventModel
            row = (
                db.query(EventModel.content)
                .filter(
                    EventModel.session_id == sid,
                    EventModel.event_type == "tool_result_quality",
                )
                .first()
            )
            assert row is not None
            content = json.loads(row[0])
            assert content["tool_name"] == "stock_assistant"
            assert content["score"] == 0.3
            assert content["grade"] == "degraded"
            assert len(content["signals"]) == 2
            assert "empty_containers" in content["signals"][0]
            assert "zero_cluster" in content["signals"][1]
        finally:
            db.close()
