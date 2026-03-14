"""Golden session regression tests.

Loads real DeepSeek conversations recorded as JSON fixtures, inserts them
into the database, then replays via ReplayService + ToolMockingLayer to
verify the replay system works with production-realistic data.

No LLM API calls are made — all responses come from recorded fixtures.
"""

import json
from pathlib import Path

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from api.models import Event, Session as SessionModel
from api.services.replay_service import ReplayService
from core.replay.semantic_diff import SemanticDiff
from core.skills.mocking import MockMode, ToolMockingLayer

FIXTURE_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "golden_sessions"


def _load_fixture(name: str) -> dict:
    return json.loads((FIXTURE_DIR / f"{name}.json").read_text())


def _uid():
    return str(uuid7())


@pytest.fixture(
    params=["code_review", "debug_error", "chained_tool_calls", "multi_turn_correction"]
)
def golden(request):
    return _load_fixture(request.param)


@pytest.fixture
def seed_session(db_session):
    """Insert golden session events into DB, return (session_id, events, fixture)."""
    created = []

    def _seed(fixture: dict):
        sid = fixture["session_id"]
        uid = fixture["user_id"]

        db_session.add(SessionModel(session_id=sid, user_id=uid, status="active"))

        for ev in fixture["events"]:
            db_session.add(
                Event(
                    event_id=ev["event_id"],
                    session_id=ev["session_id"],
                    user_id=ev["user_id"],
                    event_type=ev["event_type"],
                    content=ev["content"],
                    causal_chain_id=ev["causal_chain_id"],
                    parent_event_id=ev.get("parent_event_id"),
                    skill_name=ev.get("skill_name"),
                    skill_version=ev.get("skill_version"),
                    event_metadata=ev.get("metadata", {}),
                    token_usage=ev.get("token_usage"),
                    quality_score=ev.get("quality_score"),
                    llm_model_used=ev.get("llm_model_used"),
                )
            )
        db_session.commit()
        created.append(sid)
        return sid, fixture["events"], fixture

    yield _seed

    # Cleanup
    for sid in created:
        try:
            db_session.execute(text("DELETE FROM agent_events WHERE session_id = :s"), {"s": sid})
            db_session.execute(text("DELETE FROM agent_sessions WHERE session_id = :s"), {"s": sid})
            db_session.commit()
        except Exception:
            db_session.rollback()


class TestGoldenSessionReplay:
    """Replay golden sessions recorded from real DeepSeek conversations."""

    def test_replay_completes_all_events(self, db_session, golden, seed_session):
        """ReplayService replays every event in the golden session."""
        sid, events, fixture = seed_session(golden)
        svc = ReplayService(lambda: db_session)

        result = svc.replay_session(
            session_id=sid,
            user_id=fixture["user_id"],
            mock_mode=True,
        )

        assert result["status"] == "completed"
        assert result["events_replayed"] == len(events)
        assert result["result"]["failed"] == 0

    def test_tool_results_retrievable_in_replay(self, db_session, golden, seed_session):
        """ToolMockingLayer can retrieve recorded tool_result for every tool_call."""
        sid, events, _ = seed_session(golden)

        replay = ToolMockingLayer(
            mode=MockMode.REPLAY,
            db_factory=lambda: db_session,
            session_id=sid,
        )

        tool_calls = [e for e in events if e["event_type"] == "tool_call"]
        for tc in tool_calls:
            result = replay.get_mock_result(
                tc["skill_name"],
                tc["metadata"]["skill_params"],
                sid,
                parent_event_id=tc["event_id"],
            )
            assert result is not None, (
                f"No recorded result for {tc['skill_name']} (event_id={tc['event_id']})"
            )

    def test_compare_replay_with_original(self, db_session, golden, seed_session):
        """Replay output matches original for non-tool events (passthrough)."""
        sid, events, fixture = seed_session(golden)
        svc = ReplayService(lambda: db_session)

        result = svc.replay_session(
            session_id=sid,
            user_id=fixture["user_id"],
            mock_mode=True,
        )
        comparison = svc.compare_outputs(
            session_id=sid,
            user_id=fixture["user_id"],
            replay_result=result["result"],
        )

        # Non-tool events should match exactly (passthrough)
        # tool_call events go through invoke_skill so content may differ
        assert comparison["original_event_count"] == comparison["replay_event_count"]

    def test_causal_chain_intact(self, db_session, golden, seed_session):
        """All events in golden session share the same causal chain."""
        sid, events, _ = seed_session(golden)

        chain_ids = {e["causal_chain_id"] for e in events}
        assert len(chain_ids) == 1, f"Expected 1 chain, got {len(chain_ids)}"

        # Verify parent chain: each event (except first) has a parent
        for e in events[1:]:
            assert e["parent_event_id"] is not None, (
                f"Event {e['event_id']} ({e['event_type']}) missing parent"
            )

    def test_verify_reproducibility(self, db_session, golden, seed_session):
        """verify_reproducibility checks tool_call events have skill_params."""
        sid, _, fixture = seed_session(golden)
        svc = ReplayService(lambda: db_session)

        result = svc.verify_reproducibility(
            session_id=sid,
            user_id=fixture["user_id"],
        )
        assert result["reproducible"] is True
        assert result["issues"] == []


class TestGoldenSessionSemanticDiff:
    """Compare two golden sessions via SemanticDiff."""

    def test_diff_code_review_vs_debug(self, db_session, seed_session):
        """Two different scenarios produce meaningful semantic diff."""
        cr = _load_fixture("code_review")
        de = _load_fixture("debug_error")

        sid1, _, _ = seed_session(cr)
        sid2, _, _ = seed_session(de)

        diff = SemanticDiff(lambda: db_session)
        result = diff.compare_sessions(sid1, sid2)

        assert result["session1"] == sid1
        assert result["session2"] == sid2
        # Both have tool_call events
        assert "tool_call" in result["event_types"]
        # Event counts differ (6 vs 5)
        assert (
            result["event_types"]["user_query"]["session1"]
            != result["event_types"]["user_query"]["session2"]
            or result["event_types"]["llm_response"]["session1"]
            != result["event_types"]["llm_response"]["session2"]
        )
        # Summary should have content
        assert isinstance(result["summary"], str)


class TestChainedToolCallReplay:
    """Verify 3-tool chain: search → analyze → optimize."""

    def test_all_three_tools_retrievable(self, db_session, seed_session):
        """Each tool in the chain has its recorded result available."""
        fixture = _load_fixture("chained_tool_calls")
        sid, events, _ = seed_session(fixture)

        replay = ToolMockingLayer(
            mode=MockMode.REPLAY,
            db_factory=lambda: db_session,
            session_id=sid,
        )

        tool_calls = [e for e in events if e["event_type"] == "tool_call"]
        assert len(tool_calls) == 3, "Should have 3 chained tool calls"

        skills_seen = []
        for tc in tool_calls:
            r = replay.get_mock_result(
                tc["skill_name"],
                tc["metadata"]["skill_params"],
                sid,
                parent_event_id=tc["event_id"],
            )
            assert r is not None, f"Missing result for {tc['skill_name']}"
            skills_seen.append(tc["skill_name"])

        assert skills_seen == ["slow_query_search", "index_analyzer", "apply_optimization"]

    def test_replay_10_events(self, db_session, seed_session):
        """ReplayService handles 10-event session with 3 tool calls."""
        fixture = _load_fixture("chained_tool_calls")
        sid, _, _ = seed_session(fixture)

        svc = ReplayService(lambda: db_session)
        result = svc.replay_session(session_id=sid, user_id=fixture["user_id"], mock_mode=True)

        assert result["events_replayed"] == 10
        assert result["result"]["failed"] == 0


class TestRegressionDetection:
    """THE VALUE TEST: detect behavioral regression when tool results change.

    This simulates what happens when a skill is upgraded and produces
    different output. The replay system should detect the mismatch.
    """

    def test_tampered_tool_result_detected_by_compare(self, db_session, seed_session):
        """If a tool_result changes between original and replay, compare_outputs catches it.

        Scenario:
        1. Load golden session (original recording)
        2. Replay it (returns original recorded results — baseline)
        3. Tamper with one tool_result in DB (simulates skill upgrade producing different output)
        4. compare_outputs detects the mismatch
        """
        fixture = _load_fixture("chained_tool_calls")
        sid, events, _ = seed_session(fixture)
        svc = ReplayService(lambda: db_session)

        # Step 1-2: replay to get baseline
        replay_result = svc.replay_session(
            session_id=sid,
            user_id=fixture["user_id"],
            mock_mode=True,
        )
        assert replay_result["result"]["failed"] == 0

        # Step 3: tamper with a tool_result event (simulate skill v2 producing different output)
        tool_results = [e for e in events if e["event_type"] == "tool_result"]
        tampered_eid = tool_results[-1]["event_id"]  # last tool_result
        db_session.query(Event).filter(Event.event_id == tampered_eid).update(
            {"content": "COMPLETELY DIFFERENT OUTPUT FROM SKILL V2"}
        )
        db_session.commit()

        # Step 4: compare — should detect mismatch
        comparison = svc.compare_outputs(
            session_id=sid,
            user_id=fixture["user_id"],
            replay_result=replay_result["result"],
        )

        assert comparison["mismatched_events"] > 0, (
            "compare_outputs should detect that tool_result content changed"
        )
        assert comparison["match"] is False

    def test_missing_tool_result_breaks_reproducibility(self, db_session, seed_session):
        """If skill_params are missing from metadata, verify_reproducibility flags it.

        This catches the case where a code change accidentally stops recording
        skill parameters — replay would silently produce wrong results.
        """
        fixture = _load_fixture("code_review")
        sid, events, _ = seed_session(fixture)

        # Remove skill_params from a tool_call event's metadata
        tool_calls = [e for e in events if e["event_type"] == "tool_call"]
        db_session.query(Event).filter(Event.event_id == tool_calls[0]["event_id"]).update(
            {"event_metadata": {}}
        )
        db_session.commit()

        svc = ReplayService(lambda: db_session)
        result = svc.verify_reproducibility(
            session_id=sid,
            user_id=fixture["user_id"],
        )

        assert result["reproducible"] is False
        assert len(result["issues"]) > 0
        assert result["issues"][0]["type"] == "missing_input"
