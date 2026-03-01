"""Tests for reflect tool and server-side reflect evidence gathering.

Four layers:
1. Edge tool (mock HTTP) — verifies request/response contract
2. Server evidence (real DB) — verifies all 5 data source queries
3. Reflection learning (real DB) — verifies cross-turn Memory persistence
4. Local skill provenance — verifies data_source flows through ToolRouter
"""

import json

import pytest
from unittest.mock import MagicMock, AsyncMock

from cli.tools.reflect import ReflectTool


# ============================================================================
# Shared fixtures
# ============================================================================

@pytest.fixture
def reflect_session():
    """Create a real DB session with a realistic event trail for reflect tests.

    Yields (session_id, user_id, chain_id) and cleans up after.
    """
    from api.database import SessionLocal
    from core.events.session_manager import SessionManager
    from core.events.event_logger import EventLogger
    from sqlalchemy import text

    user_id = "reflect_tst_usr"

    mgr = SessionManager(SessionLocal())
    session = mgr.create_session(user_id=user_id)
    sid = session.session_id

    el = EventLogger(SessionLocal)
    uq = el.create_user_query(user_id=user_id, session_id=sid,
                               content="fix the bug in main.py")
    chain = uq.causal_chain_id

    el.create_stream_event(user_id=user_id, session_id=sid,
                           event_type="tool_call",
                           content=json.dumps({"name": "read_file", "tool_call_id": "tc1"}),
                           parent_event_id=uq.event_id, causal_chain_id=chain,
                           skill_name="read_file")
    el.create_stream_event(user_id=user_id, session_id=sid,
                           event_type="tool_result",
                           content=json.dumps({"name": "read_file", "result": "Error: file not found"}),
                           parent_event_id=uq.event_id, causal_chain_id=chain,
                           skill_name="read_file")
    el.create_stream_event(user_id=user_id, session_id=sid,
                           event_type="tool_call",
                           content=json.dumps({"name": "bash", "tool_call_id": "tc2"}),
                           parent_event_id=uq.event_id, causal_chain_id=chain,
                           skill_name="bash")
    el.create_stream_event(user_id=user_id, session_id=sid,
                           event_type="tool_result",
                           content=json.dumps({"name": "bash", "result": "command executed successfully"}),
                           parent_event_id=uq.event_id, causal_chain_id=chain,
                           skill_name="bash")

    yield sid, user_id, chain

    db = SessionLocal()
    for table in ("ctx_prompt_feedback", "skill_selection_events", "mem_memories",
                  "agent_events", "agent_sessions"):
        try:
            db.execute(text(f"DELETE FROM {table} WHERE session_id = :sid"), {"sid": sid})
        except Exception:
            pass
    try:
        db.execute(text("DELETE FROM mem_memories WHERE user_id = :uid AND content LIKE '%reflect%'"),
                   {"uid": user_id})
    except Exception:
        pass
    db.commit()
    db.close()


@pytest.fixture
def _provenance_classes():
    """Shared Skill classes for provenance tests — avoids duplication."""
    from core.skills.base import Skill, SkillInput, SkillOutput, SkillRequirement

    class ProvInput(SkillInput):
        query: str = ""

    class ProvOutput(SkillOutput):
        value: float = 0.0

    class WithProvenance(Skill[ProvInput, ProvOutput]):
        name = "test_with_provenance"
        version = "1.0.0"
        description = "test"
        requirements = SkillRequirement()

        async def execute(self, input: ProvInput) -> ProvOutput:
            return ProvOutput(
                success=True, value=42.0,
                data_source="test_api", data_timestamp="2026-03-01T12:00:00Z",
            )

    class WithoutProvenance(Skill[ProvInput, ProvOutput]):
        name = "test_no_provenance"
        version = "1.0.0"
        description = "test"
        requirements = SkillRequirement()

        async def execute(self, input: ProvInput) -> ProvOutput:
            return ProvOutput(success=True, value=99.0)

    return ProvInput, ProvOutput, WithProvenance, WithoutProvenance


# ============================================================================
# 1. Edge tool tests (mock HTTP)
# ============================================================================

class TestReflectTool:

    @pytest.mark.asyncio
    async def test_no_session_id(self):
        tool = ReflectTool(api_client=AsyncMock(), session_info={})
        result = json.loads(await tool.execute())
        assert "error" in result

    @pytest.mark.asyncio
    async def test_no_api_client(self):
        tool = ReflectTool(api_client=None, session_info={"session_id": "s1"})
        result = json.loads(await tool.execute())
        assert "error" in result

    @pytest.mark.asyncio
    async def test_calls_public_api(self):
        """ReflectTool uses the public get_reflect() method, not _request."""
        mock_client = AsyncMock()
        mock_client.get_reflect.return_value = {"session_id": "s1", "focus": "auto", "event_summary": []}

        tool = ReflectTool(api_client=mock_client, session_info={"session_id": "s1"})
        result = await tool.execute(focus="skill_failure", last_n=10)

        mock_client.get_reflect.assert_called_once_with("s1", focus="skill_failure", last_n=10)
        assert json.loads(result)["session_id"] == "s1"

    @pytest.mark.asyncio
    async def test_handles_server_error(self):
        mock_client = AsyncMock()
        mock_client.get_reflect.side_effect = ConnectionError("server down")
        tool = ReflectTool(api_client=mock_client, session_info={"session_id": "s1"})
        result = json.loads(await tool.execute())
        assert "server down" in result["error"]

    def test_tool_schema(self):
        schema = ReflectTool().to_openai_schema()
        assert schema["function"]["name"] == "reflect"
        assert set(schema["function"]["parameters"]["properties"]["focus"]["enum"]) == {
            "auto", "skill_failure", "unexpected_result", "data_quality"}


# ============================================================================
# 2. Server evidence — real DB queries for all 5 data sources
# ============================================================================

class TestBuildReflectEvidence:

    def test_event_summary(self, reflect_session):
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)

        assert result["session_id"] == sid
        assert len(result["event_summary"]) == 5
        assert {e["type"] for e in result["event_summary"]} == {"user_query", "tool_call", "tool_result"}

    def test_failed_tool_detected(self, reflect_session):
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)

        failed = [e for e in result["event_summary"] if e.get("failed")]
        assert len(failed) == 1
        assert failed[0]["tool_name"] == "read_file"

    def test_auto_focus_detects_failure(self, reflect_session):
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        assert _build_reflect_evidence(sid, uid, "auto", 20)["focus"] == "skill_failure"

    def test_auto_focus_detects_data_quality(self):
        """No failures → auto-focus should detect missing provenance."""
        from api.database import SessionLocal
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text

        uid = "reflect_auto_dq"
        mgr = SessionManager(SessionLocal())
        session = mgr.create_session(user_id=uid)
        sid = session.session_id

        el = EventLogger(SessionLocal)
        uq = el.create_user_query(user_id=uid, session_id=sid, content="get price")
        el.create_stream_event(
            user_id=uid, session_id=sid, event_type="tool_result",
            content=json.dumps({"name": "fetch_price", "result": "42.0"}),
            parent_event_id=uq.event_id, causal_chain_id=uq.causal_chain_id,
            skill_name="fetch_price",
        )

        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)
        assert result["focus"] == "data_quality"

        # Cleanup
        db = SessionLocal()
        db.execute(text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
        db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
        db.commit()
        db.close()

    def test_explicit_focus_preserved(self, reflect_session):
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        assert _build_reflect_evidence(sid, uid, "data_quality", 20)["focus"] == "data_quality"

    def test_last_n_limits_events(self, reflect_session):
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        assert len(_build_reflect_evidence(sid, uid, "auto", 2)["event_summary"]) <= 2

    def test_repeated_failure_hint(self, reflect_session):
        """Two failures for same tool → repeated failure diagnosis hint."""
        sid, uid, _ = reflect_session
        from api.database import SessionLocal
        from core.events.event_logger import EventLogger

        EventLogger(SessionLocal).create_stream_event(
            user_id=uid, session_id=sid, event_type="tool_result",
            content=json.dumps({"name": "read_file", "result": "Error: permission denied"}),
            skill_name="read_file",
        )

        from api.routers.chat import _build_reflect_evidence
        hints = _build_reflect_evidence(sid, uid, "auto", 20)["diagnosis_hints"]
        assert any("read_file" in h and "failed" in h for h in hints)

    def test_missing_provenance_hint(self, reflect_session):
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        hints = _build_reflect_evidence(sid, uid, "data_quality", 20)["diagnosis_hints"]
        assert any("data_source" in h for h in hints)

    def test_skill_selection_history_from_db(self, reflect_session):
        """Insert real skill_selection_events → reflect returns them."""
        sid, uid, _ = reflect_session
        from api.database import SessionLocal
        from api.models.skill import SkillSelectionEvent
        from uuid_utils import uuid7

        db = SessionLocal()
        db.add(SkillSelectionEvent(
            event_id=str(uuid7()), session_id=sid,
            user_query="fix the bug", skill_name="read_file",
            selected_skills=["read_file"], selection_method="llm_tool_choice",
            execution_success=0, execution_time_ms=150,
        ))
        db.commit()
        db.close()

        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)
        assert len(result["skill_history"]) == 1
        assert result["skill_history"][0]["skill"] == "read_file"
        assert result["skill_history"][0]["success"] is False
        assert result["skill_history"][0]["time_ms"] == 150

    def test_past_lessons_from_memory(self, reflect_session):
        """Insert real procedural memory → reflect returns it."""
        sid, uid, _ = reflect_session
        from api.database import SessionLocal
        from core.memory.store import MemoryStore
        from core.memory.types import Memory, MemoryType, TrustTier

        store = MemoryStore(SessionLocal)
        store.create(Memory(
            memory_id="", user_id=uid,
            memory_type=MemoryType.PROCEDURAL,
            content="reflect test: read_file fails on symlinks, use realpath first",
            trust_tier=TrustTier.T3_INFERRED,
            session_id=sid,
        ))

        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)
        assert any("read_file fails on symlinks" in l for l in result["past_lessons"])
        assert any("Past lesson matches" in h for h in result["diagnosis_hints"])

    def test_feedback_signals_graceful_when_table_missing(self, reflect_session):
        """When ctx_prompt_feedback table doesn't exist, feedback_signals = []."""
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)
        assert result["feedback_signals"] == []


# ============================================================================
# 3. Reflection learning — real DB Memory persistence
# ============================================================================

class TestReflectionLearningRealDB:
    """Cross-turn reflection learning with real MemoryStore writes."""

    @pytest.fixture(autouse=True)
    def _cleanup_lessons(self, reflect_session):
        """Cleanup any persisted lessons after each test."""
        sid, uid, _ = reflect_session
        yield
        from api.database import SessionLocal
        from sqlalchemy import text
        db = SessionLocal()
        try:
            db.execute(text(
                "DELETE FROM mem_memories WHERE user_id = :uid AND content LIKE '%Reflection-driven%'"
            ), {"uid": uid})
            db.commit()
        except Exception:
            pass
        finally:
            db.close()

    def test_reflect_then_retry_creates_real_memory(self, reflect_session):
        """Full path: reflect → retry → procedural memory persisted in DB."""
        sid, uid, _ = reflect_session
        from api.database import SessionLocal
        from core.agent.turn_hooks import TurnHooks
        from core.memory.store import MemoryStore
        from core.memory.types import MemoryType

        hooks = TurnHooks(SessionLocal)

        # Turn 1: LLM calls reflect
        hooks.detect_reflection_learning(
            sid, uid,
            [{"function": {"name": "reflect"}}],
            [{"name": "reflect", "result": "read_file failed: file not found"}],
        )

        # Turn 2: LLM retries with bash
        hooks.detect_reflection_learning(
            sid, uid,
            [{"function": {"name": "bash"}}],
            [{"name": "bash", "result": "ok"}],
        )

        # Verify: procedural memory was persisted in real DB
        store = MemoryStore(SessionLocal)
        memories = store.list_active(uid, MemoryType.PROCEDURAL)
        lessons = [m for m in memories if "Reflection-driven fix" in m.content]
        assert len(lessons) >= 1
        assert "bash" in lessons[0].content

    def test_no_lesson_without_reflect_first(self, reflect_session):
        """Calling a tool without prior reflect should NOT create a lesson."""
        sid, uid, _ = reflect_session
        from api.database import SessionLocal
        from core.agent.turn_hooks import TurnHooks
        from core.memory.store import MemoryStore
        from core.memory.types import MemoryType

        hooks = TurnHooks(SessionLocal)
        hooks.detect_reflection_learning(
            sid, uid,
            [{"function": {"name": "bash"}}],
            [{"name": "bash", "result": "ok"}],
        )

        store = MemoryStore(SessionLocal)
        memories = store.list_active(uid, MemoryType.PROCEDURAL)
        assert not any("Reflection-driven fix" in m.content for m in memories)


# ============================================================================
# 4. Local skill provenance — data_source flows through ToolRouter
# ============================================================================

class TestLocalSkillProvenance:
    """Verify SkillOutput.data_source survives serialization through ToolRouter."""

    @pytest.mark.asyncio
    async def test_data_source_in_tool_result(self, _provenance_classes):
        """Typed skill with data_source → ToolRouter serializes it to JSON result."""
        from cli.tools.router import ToolRouter, ToolCall
        _, _, WithProvenance, _ = _provenance_classes

        router = ToolRouter()
        router.register(WithProvenance())
        results = await router.execute([ToolCall(id="tc1", name="test_with_provenance", arguments={"query": "test"})])

        data = json.loads(results[0].result)
        assert data["data_source"] == "test_api"
        assert data["data_timestamp"] == "2026-03-01T12:00:00Z"
        assert data["value"] == 42.0

    @pytest.mark.asyncio
    async def test_empty_data_source_still_serialized(self, _provenance_classes):
        """Skill that doesn't set data_source → empty string in JSON (not omitted)."""
        from cli.tools.router import ToolRouter, ToolCall
        _, _, _, WithoutProvenance = _provenance_classes

        router = ToolRouter()
        router.register(WithoutProvenance())
        results = await router.execute([ToolCall(id="tc1", name="test_no_provenance", arguments={"query": "x"})])

        data = json.loads(results[0].result)
        # data_source="" is not None, so exclude_none=True keeps it
        assert "data_source" in data
        assert data["data_source"] == ""


# ============================================================================
# 5. Session cache
# ============================================================================

class TestSessionCache:

    def test_peek_returns_none_for_missing(self):
        from api.routers.chat import _peek_session_entry
        assert _peek_session_entry("nonexistent_session_xyz") is None
