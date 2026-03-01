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

        mock_client.get_reflect.assert_called_once_with("s1", focus="skill_failure", last_n=10, question="")
        assert json.loads(result)["session_id"] == "s1"

    @pytest.mark.asyncio
    async def test_passes_question_param(self):
        """question parameter is forwarded to the API client."""
        mock_client = AsyncMock()
        mock_client.get_reflect.return_value = {"session_id": "s1"}

        tool = ReflectTool(api_client=mock_client, session_info={"session_id": "s1"})
        await tool.execute(focus="tool_selection", question="why not list_prs?")

        mock_client.get_reflect.assert_called_once_with(
            "s1", focus="tool_selection", last_n=20, question="why not list_prs?")

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
            "auto", "skill_failure", "unexpected_result", "data_quality",
            "tool_selection", "history"}


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

    def test_tool_selection_returns_usage_counts(self, reflect_session):
        """focus=tool_selection includes tool_usage_counts from session events."""
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "tool_selection", 20)
        assert "tool_usage_counts" in result
        # reflect_session fixture creates read_file and bash tool_call events
        assert "read_file" in result["tool_usage_counts"]
        assert "bash" in result["tool_usage_counts"]
        assert "edge_tools" in result

    def test_tool_selection_question_hint(self, reflect_session):
        """question param generates hint when matching a cloud skill name."""
        sid, uid, _ = reflect_session
        from unittest.mock import patch, MagicMock
        from api.routers.chat import _build_reflect_evidence

        mock_skill = MagicMock()
        mock_skill.name = "list_prs"
        mock_skill.description = "List PRs"
        mock_skill.to_openai_schema.return_value = {
            "function": {"parameters": {"type": "object", "properties": {"repo": {"type": "string"}}}}
        }
        mock_reg = MagicMock()
        mock_reg._skills = {"list_prs": mock_skill}

        with patch("api.routers.chat._get_shared_skill_registry", return_value=mock_reg):
            result = _build_reflect_evidence(sid, uid, "tool_selection", 20, question="list_prs")

        assert any("list_prs" in h for h in result["diagnosis_hints"])

    def test_history_returns_related_queries(self):
        """focus=history finds similar queries from other sessions."""
        from api.database import SessionLocal
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text
        from api.routers.chat import _build_reflect_evidence

        uid = "reflect_hist_usr"
        mgr = SessionManager(SessionLocal())

        # Create an older session with a matching query
        old_session = mgr.create_session(user_id=uid)
        el = EventLogger(SessionLocal)
        el.create_user_query(user_id=uid, session_id=old_session.session_id,
                             content="database connection pool exhausted yesterday")

        # Current session — keywords (len>3): "database", "connection", "pool", "exhausted"
        # Old query contains all of them → AND match succeeds.
        cur_session = mgr.create_session(user_id=uid)
        el.create_user_query(user_id=uid, session_id=cur_session.session_id,
                             content="database connection pool exhausted again")

        try:
            result = _build_reflect_evidence(cur_session.session_id, uid, "history", 20)
            assert "related_history" in result
            assert len(result["related_history"]) >= 1
            assert result["related_history"][0]["session_id"] == old_session.session_id
        finally:
            db = SessionLocal()
            for sid in (old_session.session_id, cur_session.session_id):
                db.execute(text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
                db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()
            db.close()

    def test_history_escapes_like_wildcards(self):
        """LIKE wildcards in user input must be escaped — '%' should not match everything."""
        from api.database import SessionLocal
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text
        from api.routers.chat import _build_reflect_evidence

        uid = "reflect_esc_usr"
        mgr = SessionManager(SessionLocal())
        el = EventLogger(SessionLocal)

        # Old session with normal content (no % or _)
        old_session = mgr.create_session(user_id=uid)
        el.create_user_query(user_id=uid, session_id=old_session.session_id,
                             content="normal query about servers")

        # Current session: query contains LIKE wildcards.
        # Without escaping, "100%" would become LIKE '%100%%' matching everything.
        # With escaping, it becomes LIKE '%100\%%' matching only literal "100%".
        cur_session = mgr.create_session(user_id=uid)
        el.create_user_query(user_id=uid, session_id=cur_session.session_id,
                             content="100% match_test query")

        try:
            result = _build_reflect_evidence(cur_session.session_id, uid, "history", 20)
            assert "related_history" in result
            # Old session does NOT contain "100%" or "match_test" →
            # with proper escaping it must NOT appear in results.
            matched_sids = {r["session_id"] for r in result["related_history"]}
            assert old_session.session_id not in matched_sids
        finally:
            db = SessionLocal()
            for sid in (old_session.session_id, cur_session.session_id):
                db.execute(text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
                db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()
            db.close()

    def test_token_summary_present(self, reflect_session):
        """reflect response includes token_summary with zeroes when no LLM events."""
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)
        ts = result["token_summary"]
        assert ts["llm_calls"] == 0
        assert ts["total_prompt_tokens"] == 0
        assert ts["total_completion_tokens"] == 0
        assert ts["total_tokens"] == 0
        assert ts["by_model"] == {}

    def test_token_summary_accumulates_llm_events(self):
        """LLM response events with token_usage are accumulated into token_summary."""
        from api.database import SessionLocal
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text
        from api.routers.chat import _build_reflect_evidence

        uid = "reflect_tok_usr"
        mgr = SessionManager(SessionLocal())
        session = mgr.create_session(user_id=uid)
        sid = session.session_id

        el = EventLogger(SessionLocal)
        uq = el.create_user_query(user_id=uid, session_id=sid, content="hello")

        # Insert LLM response events with token_usage
        db = SessionLocal()
        from api.models.agent import Event as EventModel
        import uuid
        for i, (p, c) in enumerate([(1000, 50), (2000, 100)]):
            db.add(EventModel(
                event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
                agent_id="test", event_type="llm_response", content=f"response {i}",
                causal_chain_id=uq.causal_chain_id,
                llm_model_used="test-model",
                token_usage=json.dumps({"prompt_tokens": p, "completion_tokens": c}),
            ))
        db.commit()
        db.close()

        try:
            result = _build_reflect_evidence(sid, uid, "auto", 20)
            ts = result["token_summary"]
            assert ts["total_prompt_tokens"] == 3000
            assert ts["total_completion_tokens"] == 150
            assert ts["total_tokens"] == 3150
            assert ts["llm_calls"] == 2
            assert ts["by_model"]["test-model"]["calls"] == 2
            assert ts["by_model"]["test-model"]["prompt_tokens"] == 3000
        finally:
            db = SessionLocal()
            db.execute(text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
            db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()
            db.close()

    def test_tool_quality_summary_present(self, reflect_session):
        """reflect response includes tool_quality_summary (empty when no quality events)."""
        sid, uid, _ = reflect_session
        from api.routers.chat import _build_reflect_evidence
        result = _build_reflect_evidence(sid, uid, "auto", 20)
        assert result["tool_quality_summary"] == []

    def test_tool_quality_summary_surfaces_degraded(self):
        """tool_result_quality events with non-complete grade appear in summary."""
        from api.database import SessionLocal
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text
        from api.routers.chat import _build_reflect_evidence

        uid = "reflect_tq_usr"
        mgr = SessionManager(SessionLocal())
        session = mgr.create_session(user_id=uid)
        sid = session.session_id

        el = EventLogger(SessionLocal)
        uq = el.create_user_query(user_id=uid, session_id=sid, content="analyze stock")

        # Insert a tool_result_quality event
        db = SessionLocal()
        from api.models.agent import Event as EventModel
        import uuid
        db.add(EventModel(
            event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
            agent_id="test", event_type="tool_result_quality", content="",
            causal_chain_id=uq.causal_chain_id,
            event_metadata={
                "tool_name": "stock_assistant",
                "quality_score": 0.35,
                "quality_grade": "degraded",
                "missing_fields": ["technical_indicators", "trend_analysis"],
            },
        ))
        # Insert a complete one — should NOT appear in summary
        db.add(EventModel(
            event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
            agent_id="test", event_type="tool_result_quality", content="",
            causal_chain_id=uq.causal_chain_id,
            event_metadata={
                "tool_name": "bash",
                "quality_score": 1.0,
                "quality_grade": "complete",
                "missing_fields": [],
            },
        ))
        db.commit()
        db.close()

        try:
            result = _build_reflect_evidence(sid, uid, "auto", 20)
            tqs = result["tool_quality_summary"]
            assert len(tqs) == 1
            assert tqs[0]["tool"] == "stock_assistant"
            assert tqs[0]["grade"] == "degraded"
            assert tqs[0]["score"] == 0.35
            assert "technical_indicators" in tqs[0]["missing_fields"]
        finally:
            db = SessionLocal()
            db.execute(text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
            db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()
            db.close()

    def test_high_token_usage_hint(self):
        """When total tokens > 50K, a diagnosis hint is generated."""
        from api.database import SessionLocal
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text
        from api.routers.chat import _build_reflect_evidence

        uid = "reflect_hightok"
        mgr = SessionManager(SessionLocal())
        session = mgr.create_session(user_id=uid)
        sid = session.session_id

        el = EventLogger(SessionLocal)
        uq = el.create_user_query(user_id=uid, session_id=sid, content="big query")

        db = SessionLocal()
        from api.models.agent import Event as EventModel
        import uuid
        db.add(EventModel(
            event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
            agent_id="test", event_type="llm_response", content="big response",
            causal_chain_id=uq.causal_chain_id, llm_model_used="gpt-4",
            token_usage=json.dumps({"prompt_tokens": 60000, "completion_tokens": 2000}),
        ))
        db.commit()
        db.close()

        try:
            result = _build_reflect_evidence(sid, uid, "auto", 20)
            assert any("token" in h.lower() for h in result["diagnosis_hints"])
        finally:
            db = SessionLocal()
            db.execute(text("DELETE FROM agent_events WHERE session_id = :sid"), {"sid": sid})
            db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
            db.commit()
            db.close()


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


# ============================================================================
# 6. EXPLAIN output
# ============================================================================

class TestPrintExplain:

    def test_basic_output(self):
        """_print_explain writes structured trace to the given file object."""
        import io
        from cli.edge_chat_loop import _print_explain

        buf = io.StringIO()
        _print_explain([{
            "turn": 0, "total_ms": 500,
            "prompt_tokens": 100, "completion_tokens": 50,
            "steps": [
                {"step": "llm", "loop": 0, "duration_ms": 400, "in": 100, "out": 50, "tool_calls": 1},
                {"step": "cloud_skill", "name": "list_prs", "duration_ms": 80, "in_bytes": 20, "out_bytes": 300},
            ],
        }], file=buf)

        output = buf.getvalue()
        assert "EXPLAIN" in output
        assert "Turn 0" in output
        assert "LLM" in output
        assert "list_prs" in output
        assert "Total: 500ms" in output

    def test_none_tokens_shown_as_unknown(self):
        """When token counts are None (no usage from provider), show '?'."""
        import io
        from cli.edge_chat_loop import _print_explain

        buf = io.StringIO()
        _print_explain([{
            "turn": 0, "total_ms": 200,
            "prompt_tokens": 0, "completion_tokens": 0,
            "steps": [
                {"step": "llm", "loop": 0, "duration_ms": 200, "in": None, "out": None, "tool_calls": 0},
            ],
        }], file=buf)

        output = buf.getvalue()
        assert "in=?" in output
        assert "out=?" in output

    def test_defaults_to_stderr(self):
        """When no file is given, writes to stderr (smoke test — just ensure no crash)."""
        import io
        import sys
        from cli.edge_chat_loop import _print_explain

        old_stderr = sys.stderr
        sys.stderr = io.StringIO()
        try:
            _print_explain([{"turn": 0, "total_ms": 10, "prompt_tokens": 0, "completion_tokens": 0, "steps": []}])
            assert "EXPLAIN" in sys.stderr.getvalue()
        finally:
            sys.stderr = old_stderr


class TestEscapeLike:
    """Unit tests for _escape_like helper."""

    def test_escapes_percent(self):
        from api.routers.chat import _escape_like
        assert _escape_like("100%") == "100\\%"

    def test_escapes_underscore(self):
        from api.routers.chat import _escape_like
        assert _escape_like("match_test") == "match\\_test"

    def test_escapes_backslash(self):
        from api.routers.chat import _escape_like
        assert _escape_like("a\\b") == "a\\\\b"

    def test_plain_text_unchanged(self):
        from api.routers.chat import _escape_like
        assert _escape_like("hello world") == "hello world"


class TestExplainSSE:
    """Verify explain=True produces an explain event in the SSE stream."""

    @pytest.fixture
    def client(self):
        import os
        os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
        os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)
        from fastapi.testclient import TestClient
        from api.main import app
        return TestClient(app)

    @pytest.fixture
    def db(self):
        from api.database import SessionLocal
        db = SessionLocal()
        yield db
        db.close()

    def test_explain_event_in_stream(self, client, db):
        """POST /chat/turn with explain=True → SSE stream contains type=explain."""
        from unittest.mock import patch
        from tests.conftest import get_auth_headers, parse_sse_events, fake_llm_stream

        headers = get_auth_headers(client, db, username="explain_usr", user_id="explain_uid", email="ex@t.com")

        stream = fake_llm_stream([
            {"type": "text", "content": "hello"},
            {"type": "usage", "prompt": 10, "completion": 5},
        ])

        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=stream):
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
                "edge_tools": [{"type": "function", "function": {"name": "bash", "description": "sh", "parameters": {}}}],
                "explain": True,
            }, headers=headers)

        assert resp.status_code == 200
        events = parse_sse_events(resp.text)
        explain_events = [e for e in events if e.get("type") == "explain"]
        assert len(explain_events) == 1
        ex = explain_events[0]
        assert "total_ms" in ex
        assert ex["prompt_tokens"] == 10
        assert ex["completion_tokens"] == 5
        assert isinstance(ex["steps"], list)

    def test_explain_none_tokens_without_usage(self, client, db):
        """When LLM doesn't send usage chunk, explain tokens are null."""
        from unittest.mock import patch
        from tests.conftest import get_auth_headers, parse_sse_events, fake_llm_stream

        headers = get_auth_headers(client, db, username="explain_nu", user_id="explain_nuid", email="en@t.com")

        # No usage chunk in stream
        stream = fake_llm_stream([{"type": "text", "content": "hi"}])

        with patch("core.llm.client.LLMClient.chat_with_tools_stream", return_value=stream):
            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "hi"}],
                "edge_tools": [{"type": "function", "function": {"name": "bash", "description": "sh", "parameters": {}}}],
                "explain": True,
            }, headers=headers)

        assert resp.status_code == 200
        events = parse_sse_events(resp.text)
        explain_events = [e for e in events if e.get("type") == "explain"]
        assert len(explain_events) == 1
        assert explain_events[0]["prompt_tokens"] is None
        assert explain_events[0]["completion_tokens"] is None
