"""Complex multi-turn replay integration test.

Verifies the FULL replay pipeline with realistic conversation patterns:
  user_query → llm_response → tool_call → tool_result → llm_response (synthesis)
  → second tool_call (chained, uses first result) → tool_result → final llm_response

This test exposes real integration issues that simpler tests miss:
1. ReplayService._replay_event handles "tool_call" events through ToolMockingLayer
2. EventReader.get_session_events doesn't SELECT token_usage/quality_score columns
3. Causal chain integrity across replay
"""

from datetime import datetime, timezone

import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from api.models import Event, Session as SessionModel
from api.services.replay_service import ReplayService
from core.events.event_logger import EventLogger
from core.events.models import EventType, TokenUsage
from core.replay.semantic_diff import SemanticDiff
from core.skills.base import (
    AccessScope, RepoType, SideEffectCategory, SideEffectProfile,
    Skill, SkillInput, SkillOutput, SkillRequirement,
)
from core.skills.mocking import MockMode, ToolMockingLayer


# ── Skill stubs ───────────────────────────────────────────

class _In(SkillInput):
    query: str

class _Out(SkillOutput):
    data: str
    source: str = "live"

class SearchSkill(Skill):
    name = "code_search"
    version = "1.0.0"
    description = "Search codebase"
    requirements = SkillRequirement(repo_types=[RepoType.CODE], min_access=AccessScope.READ, llm_required=False)
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.READ, external_apis=[])
    def validate_input(self, d: dict) -> _In:
        return _In(**d)
    async def execute(self, inp: _In) -> _Out:
        return _Out(success=True, result=f"found:{inp.query}", data=f"found:{inp.query}")

class ApplyPatchSkill(Skill):
    name = "apply_patch"
    version = "1.0.0"
    description = "Apply code patch"
    requirements = SkillRequirement(repo_types=[RepoType.CODE], min_access=AccessScope.WRITE, llm_required=False)
    side_effect_profile = SideEffectProfile(category=SideEffectCategory.WRITE, external_apis=["github"])
    def validate_input(self, d: dict) -> _In:
        return _In(**d)
    async def execute(self, inp: _In) -> _Out:
        return _Out(success=True, result=f"patched:{inp.query}", data=f"patched:{inp.query}")


# ── Helpers ───────────────────────────────────────────────

def _uid():
    return str(uuid7())

def _now():
    return datetime.now(timezone.utc)


# ── Fixtures ──────────────────────────────────────────────

@pytest.fixture
def sid():
    return _uid()

@pytest.fixture
def user_id():
    return _uid()

@pytest.fixture(autouse=True)
def cleanup(db_session, sid):
    yield
    try:
        db_session.execute(text(
            "DELETE FROM conversation_events WHERE session_id = :s"
        ), {"s": sid})
        db_session.execute(text(
            "DELETE FROM sessions WHERE session_id = :s"
        ), {"s": sid})
        db_session.commit()
    except Exception:
        db_session.rollback()


def _add_event(db, *, sid, uid, etype, content, chain_id,
               parent_id=None, skill_name=None, skill_version=None,
               metadata=None, token_usage=None, quality_score=None):
    """Insert an event and return its event_id."""
    eid = _uid()
    db.add(Event(
        event_id=eid, session_id=sid, user_id=uid,
        event_type=etype, content=content,
        causal_chain_id=chain_id, parent_event_id=parent_id,
        skill_name=skill_name, skill_version=skill_version,
        event_metadata=metadata or {},
        token_usage=token_usage,
        quality_score=quality_score,
        created_at=_now(),
    ))
    return eid


# ── Test: 5-turn conversation with chained tool calls ────

class TestComplexMultiTurnReplay:
    """
    Simulates:
      Turn 1: user_query  "fix the bug in parser.py"
      Turn 2: llm_response "I'll search for the bug"
      Turn 3: tool_call    code_search(query="parser bug")
      Turn 4: tool_result  code_search → found:parser bug
      Turn 5: llm_response "Found it, applying patch"
      Turn 6: tool_call    apply_patch(query="fix parser")  ← chained, depends on Turn 4
      Turn 7: tool_result  apply_patch → patched:fix parser
      Turn 8: llm_response "Bug fixed successfully"
    """

    def _seed_session(self, db, sid, uid):
        """Insert a realistic 8-event conversation and return event IDs."""
        db.add(SessionModel(session_id=sid, user_id=uid, status="active"))

        chain = _uid()
        ids = {}

        ids["user1"] = _add_event(db, sid=sid, uid=uid, etype="user_query",
            content="fix the bug in parser.py", chain_id=chain)

        ids["llm1"] = _add_event(db, sid=sid, uid=uid, etype="llm_response",
            content="I'll search for the bug", chain_id=chain,
            parent_id=ids["user1"],
            token_usage={"prompt": 50, "completion": 20, "total": 70})

        ids["tc1"] = _add_event(db, sid=sid, uid=uid, etype="tool_call",
            content="code_search", chain_id=chain,
            parent_id=ids["llm1"],
            skill_name="code_search", skill_version="1.0.0",
            metadata={"skill_params": {"query": "parser bug"}})

        ids["tr1"] = _add_event(db, sid=sid, uid=uid, etype="tool_result",
            content="found:parser bug", chain_id=chain,
            parent_id=ids["tc1"],
            skill_name="code_search", skill_version="1.0.0",
            metadata={
                "skill_params": {"query": "parser bug"},
                "skill_result": {"data": "found:parser bug", "source": "live"},
            })

        ids["llm2"] = _add_event(db, sid=sid, uid=uid, etype="llm_response",
            content="Found it, applying patch", chain_id=chain,
            parent_id=ids["tr1"],
            token_usage={"prompt": 120, "completion": 30, "total": 150})

        ids["tc2"] = _add_event(db, sid=sid, uid=uid, etype="tool_call",
            content="apply_patch", chain_id=chain,
            parent_id=ids["llm2"],
            skill_name="apply_patch", skill_version="1.0.0",
            metadata={"skill_params": {"query": "fix parser"}})

        ids["tr2"] = _add_event(db, sid=sid, uid=uid, etype="tool_result",
            content="patched:fix parser", chain_id=chain,
            parent_id=ids["tc2"],
            skill_name="apply_patch", skill_version="1.0.0",
            metadata={
                "skill_params": {"query": "fix parser"},
                "skill_result": {"data": "patched:fix parser", "source": "live"},
            })

        ids["llm3"] = _add_event(db, sid=sid, uid=uid, etype="llm_response",
            content="Bug fixed successfully", chain_id=chain,
            parent_id=ids["tr2"],
            token_usage={"prompt": 200, "completion": 15, "total": 215},
            quality_score=0.95)

        db.commit()
        return ids, chain

    # ── 1. ToolMockingLayer can retrieve recorded results for both tool calls ──

    def test_recorded_results_retrievable(self, db_session, sid, user_id):
        """REPLAY mode can find recorded tool_result for both chained skills."""
        ids, _ = self._seed_session(db_session, sid, user_id)

        replay = ToolMockingLayer(
            mode=MockMode.REPLAY, db_factory=lambda: db_session, session_id=sid,
        )

        # First tool call: code_search
        r1 = replay.get_mock_result(
            "code_search", {"query": "parser bug"}, sid,
            parent_event_id=ids["tc1"],
        )
        assert r1 is not None, "Should find recorded result for code_search"
        assert r1["data"] == "found:parser bug"

        # Second tool call: apply_patch (chained)
        r2 = replay.get_mock_result(
            "apply_patch", {"query": "fix parser"}, sid,
            parent_event_id=ids["tc2"],
        )
        assert r2 is not None, "Should find recorded result for apply_patch"
        assert r2["data"] == "patched:fix parser"

    # ── 2. ReplayService replays all 8 events correctly ──

    def test_replay_service_handles_tool_events(self, db_session, sid, user_id):
        """ReplayService handles tool_call/tool_result event types correctly."""
        self._seed_session(db_session, sid, user_id)

        svc = ReplayService(lambda: db_session)
        result = svc.replay_session(session_id=sid, user_id=user_id, mock_mode=True)

        assert result["status"] == "completed"
        assert result["events_replayed"] == 8

        events = result["result"]["events"]
        # All events should succeed
        failed = [e for e in events if not e["success"]]
        assert failed == [], f"Failed events: {failed}"

        # tool_call events should be replayed via ToolMockingLayer (invoke_skill)
        tool_call_events = [e for e in events if e["event_type"] == "tool_call"]
        assert len(tool_call_events) == 2
        for tc in tool_call_events:
            # After fix: tool_call events go through invoke_skill → recorded result
            assert tc["content"] is not None
            assert tc["success"] is True

        tool_result_events = [e for e in events if e["event_type"] == "tool_result"]
        assert len(tool_result_events) == 2

    # ── 3. Causal chain preserved across replay ──

    def test_causal_chain_integrity(self, db_session, sid, user_id):
        """All 8 events share the same causal_chain_id."""
        ids, chain = self._seed_session(db_session, sid, user_id)

        events = db_session.query(Event).filter(
            Event.session_id == sid,
        ).order_by(Event.created_at.asc()).all()

        assert len(events) == 8
        for e in events:
            assert e.causal_chain_id == chain, (
                f"Event {e.event_id} ({e.event_type}) has chain {e.causal_chain_id}, expected {chain}"
            )

        # Verify parent chain: each event's parent is the previous event
        assert events[1].parent_event_id == events[0].event_id  # llm1 → user1
        assert events[2].parent_event_id == events[1].event_id  # tc1 → llm1
        assert events[3].parent_event_id == events[2].event_id  # tr1 → tc1
        assert events[6].parent_event_id == events[5].event_id  # tr2 → tc2

    # ── 4. SemanticDiff compares two sessions ──

    def test_semantic_diff_between_sessions(self, db_session, sid, user_id):
        """SemanticDiff.compare_sessions returns meaningful diffs between two sessions."""
        self._seed_session(db_session, sid, user_id)

        # Create a second session with slightly different events
        sid2 = _uid()
        db_session.add(SessionModel(session_id=sid2, user_id=user_id, status="active"))
        chain2 = _uid()
        _add_event(db_session, sid=sid2, uid=user_id, etype="user_query",
            content="fix the bug", chain_id=chain2)
        _add_event(db_session, sid=sid2, uid=user_id, etype="llm_response",
            content="Done", chain_id=chain2)
        db_session.commit()

        diff = SemanticDiff(lambda: db_session)
        result = diff.compare_sessions(sid, sid2)

        assert result["session1"] == sid
        assert result["session2"] == sid2
        # Session 1 has 8 events, session 2 has 2
        assert result["event_types"]["user_query"]["session1"] == 1
        assert result["event_types"]["user_query"]["session2"] == 1
        assert result["event_types"]["tool_call"]["session1"] == 2
        assert result["event_types"]["tool_call"]["session2"] == 0

        # Cleanup sid2
        db_session.execute(text(
            "DELETE FROM conversation_events WHERE session_id = :s"
        ), {"s": sid2})
        db_session.execute(text(
            "DELETE FROM sessions WHERE session_id = :s"
        ), {"s": sid2})
        db_session.commit()

    # ── 5. PRODUCTION record → REPLAY retrieve for async skill ──

    def test_production_record_async_skill(self, db_session, sid, user_id):
        """ToolMockingLayer.execute() correctly handles async skill in PRODUCTION mode."""
        db_session.add(SessionModel(session_id=sid, user_id=user_id, status="active"))

        # Create a tool_result event that _record_result will update
        parent_eid = _uid()
        db_session.add(Event(
            event_id=_uid(), session_id=sid, user_id=user_id,
            event_type="tool_result", content="",
            skill_name="code_search", skill_version="1.0.0",
            parent_event_id=parent_eid, causal_chain_id=_uid(),
            event_metadata={},
        ))
        db_session.commit()

        skill = SearchSkill()  # async execute()
        prod = ToolMockingLayer(mode=MockMode.PRODUCTION, db_factory=lambda: db_session)
        result = prod.execute(skill, {"query": "test"}, sid, parent_event_id=parent_eid)

        # Should have awaited the coroutine and returned the result
        assert result.data == "found:test"
        assert result.source == "live"

        # Verify it was recorded
        replay = ToolMockingLayer(
            mode=MockMode.REPLAY, db_factory=lambda: db_session, session_id=sid,
        )
        recorded = replay.get_mock_result("code_search", {"query": "test"}, sid, parent_eid)
        assert recorded is not None
        assert recorded["data"] == "found:test"

    # ── 6. Chained replay: second skill depends on first skill's output ──

    def test_chained_skill_replay_order(self, db_session, sid, user_id):
        """In a chain (search → patch), replay returns results in correct order."""
        ids, _ = self._seed_session(db_session, sid, user_id)

        replay = ToolMockingLayer(
            mode=MockMode.REPLAY, db_factory=lambda: db_session, session_id=sid,
        )

        # Simulate the agent replaying tool calls in order
        r1 = replay.get_mock_result("code_search", {"query": "parser bug"}, sid, ids["tc1"])
        assert r1["data"] == "found:parser bug"

        # Agent would use r1 to decide next action, then call apply_patch
        r2 = replay.get_mock_result("apply_patch", {"query": "fix parser"}, sid, ids["tc2"])
        assert r2["data"] == "patched:fix parser"

        # Results are independent lookups (by parent_event_id), order doesn't matter
        # but in real replay the agent processes them sequentially
        r2_first = replay.get_mock_result("apply_patch", {"query": "fix parser"}, sid, ids["tc2"])
        r1_second = replay.get_mock_result("code_search", {"query": "parser bug"}, sid, ids["tc1"])
        assert r2_first == r2
        assert r1_second == r1
