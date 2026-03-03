"""Tests for reflect tool and server-side reflect evidence gathering.

Four layers:
1. Edge tool (mock HTTP) — verifies request/response contract
2. Server evidence (real DB) — verifies all data source queries
3. Reflection learning (real DB) — verifies cross-turn Memory persistence
4. Local skill provenance — verifies data_source flows through ToolRouter
"""

import json
import uuid

import pytest
from unittest.mock import MagicMock, AsyncMock

from cli.tools.reflect import ReflectTool


# ============================================================================
# Shared fixtures
# ============================================================================

@pytest.fixture
def reflect_session(db_factory, db_session):
    """Create a real DB session with a realistic event trail for reflect tests.

    Yields (session_id, user_id, chain_id) and cleans up via ORM after.
    """
    from core.events.session_manager import SessionManager
    from core.events.event_logger import EventLogger

    user_id = "reflect_tst_usr"

    mgr = SessionManager(db_session)
    session = mgr.create_session(user_id=user_id)
    sid = session.session_id

    el = EventLogger(db_factory)
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

    # ORM cleanup
    from api.models.agent import Event as EventModel, Session as SessionModel
    from api.models.skill import SkillSelectionEvent
    from api.models.context import PromptFeedback
    for model in (PromptFeedback, SkillSelectionEvent, EventModel, SessionModel):
        try:
            db_session.query(model).filter(model.session_id == sid).delete()
        except Exception:
            pass
    from api.models.memory import MemoryRecord
    try:
        db_session.query(MemoryRecord).filter(MemoryRecord.user_id == user_id).delete()
    except Exception:
        pass
    db_session.commit()


@pytest.fixture
def reflect_svc(db_factory):
    """ReflectService wired to test db_factory."""
    from core.agent.reflect_service import ReflectService
    return ReflectService(db_factory=db_factory)


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
# Helper to create a temp session with events and auto-cleanup
# ============================================================================

@pytest.fixture
def make_session(db_factory, db_session):
    """Factory fixture: create a session with events, auto-cleanup after test."""
    created_sids = []

    def _make(user_id, events=None):
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger

        mgr = SessionManager(db_session)
        session = mgr.create_session(user_id=user_id)
        sid = session.session_id
        created_sids.append((sid, user_id))

        if events:
            el = EventLogger(db_factory)
            uq = el.create_user_query(user_id=user_id, session_id=sid,
                                       content=events[0] if isinstance(events[0], str) else events[0]["content"])
            for evt in events[1:]:
                if isinstance(evt, str):
                    el.create_user_query(user_id=user_id, session_id=sid, content=evt)
                elif isinstance(evt, dict):
                    el.create_stream_event(
                        user_id=user_id, session_id=sid,
                        event_type=evt.get("type", "tool_result"),
                        content=json.dumps(evt.get("content_json", {})),
                        parent_event_id=uq.event_id,
                        causal_chain_id=uq.causal_chain_id,
                        skill_name=evt.get("skill_name"),
                    )
        return sid

    yield _make

    from api.models.agent import Event as EventModel, Session as SessionModel
    for sid, uid in created_sids:
        try:
            db_session.query(EventModel).filter(EventModel.session_id == sid).delete()
            db_session.query(SessionModel).filter(SessionModel.session_id == sid).delete()
        except Exception:
            pass
    db_session.commit()


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
        mock_client = AsyncMock()
        mock_client.get_reflect.return_value = {"session_id": "s1", "focus": "auto", "event_summary": []}

        tool = ReflectTool(api_client=mock_client, session_info={"session_id": "s1"})
        result = await tool.execute(focus="skill_failure", last_n=10)

        mock_client.get_reflect.assert_called_once_with("s1", focus="skill_failure", last_n=10, question="")
        assert json.loads(result)["session_id"] == "s1"

    @pytest.mark.asyncio
    async def test_passes_question_param(self):
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
            "tool_selection", "history", "performance"}

    def test_description_mentions_all_focus_values(self):
        """Every focus value mentioned in description must exist in enum (no stale references)."""
        tool = ReflectTool()
        enum_vals = set(tool.parameters["properties"]["focus"]["enum"])
        all_text = tool.description + tool.parameters["properties"]["focus"]["description"]
        # Extract focus='xxx' patterns from description
        import re
        mentioned = set(re.findall(r"'(\w+)'", all_text))
        # Every mentioned value that looks like a focus must be in enum
        for val in mentioned & {"performance", "skill_failure", "unexpected_result",
                                "data_quality", "tool_selection", "history", "auto"}:
            assert val in enum_vals, f"focus='{val}' in description but not in enum"

    def test_description_boundary_with_get_agent_info(self):
        """reflect description must direct token/context queries to get_agent_info."""
        tool = ReflectTool()
        assert "get_agent_info" in tool.description


# ============================================================================
# 2. Server evidence — real DB queries via ReflectService
# ============================================================================

class TestBuildReflectEvidence:

    def test_event_summary(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        result = reflect_svc.build_evidence(sid, uid, "auto", 20)
        assert result["session_id"] == sid
        assert len(result["event_summary"]) == 5
        assert {e["type"] for e in result["event_summary"]} == {"user_query", "tool_call", "tool_result"}

    def test_failed_tool_detected(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        result = reflect_svc.build_evidence(sid, uid, "auto", 20)
        failed = [e for e in result["event_summary"] if e.get("failed")]
        assert len(failed) == 1
        assert failed[0]["tool_name"] == "read_file"

    def test_auto_focus_detects_failure(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        assert reflect_svc.build_evidence(sid, uid, "auto", 20)["focus"] == "skill_failure"

    def test_auto_focus_detects_data_quality(self, make_session, reflect_svc):
        uid = "reflect_auto_dq"
        sid = make_session(uid, [
            "get price",
            {"type": "tool_result", "content_json": {"name": "fetch_price", "result": "42.0"}, "skill_name": "fetch_price"},
        ])
        result = reflect_svc.build_evidence(sid, uid, "auto", 20)
        assert result["focus"] == "data_quality"

    def test_explicit_focus_preserved(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        assert reflect_svc.build_evidence(sid, uid, "data_quality", 20)["focus"] == "data_quality"

    def test_last_n_limits_events(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        assert len(reflect_svc.build_evidence(sid, uid, "auto", 2)["event_summary"]) <= 2

    def test_repeated_failure_hint(self, reflect_session, db_factory, reflect_svc):
        sid, uid, _ = reflect_session
        from core.events.event_logger import EventLogger
        EventLogger(db_factory).create_stream_event(
            user_id=uid, session_id=sid, event_type="tool_result",
            content=json.dumps({"name": "read_file", "result": "Error: permission denied"}),
            skill_name="read_file",
        )
        hints = reflect_svc.build_evidence(sid, uid, "auto", 20)["diagnosis_hints"]
        assert any("read_file" in h and "failed" in h for h in hints)

    def test_missing_provenance_hint(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        hints = reflect_svc.build_evidence(sid, uid, "data_quality", 20)["diagnosis_hints"]
        assert any("data_source" in h for h in hints)

    def test_skill_selection_history_from_db(self, reflect_session, db_session, reflect_svc):
        sid, uid, _ = reflect_session
        from api.models.skill import SkillSelectionEvent
        from uuid_utils import uuid7
        db_session.add(SkillSelectionEvent(
            event_id=str(uuid7()), session_id=sid,
            user_query="fix the bug", skill_name="read_file",
            selected_skills=["read_file"], selection_method="llm_tool_choice",
            execution_success=0, execution_time_ms=150,
        ))
        db_session.commit()

        result = reflect_svc.build_evidence(sid, uid, "auto", 20)
        assert len(result["skill_history"]) == 1
        assert result["skill_history"][0]["skill"] == "read_file"
        assert result["skill_history"][0]["success"] is False
        assert result["skill_history"][0]["time_ms"] == 150

    def test_past_lessons_from_memory(self, reflect_session, db_factory, reflect_svc):
        sid, uid, _ = reflect_session
        from core.memory.store import MemoryStore
        from core.memory.types import Memory, MemoryType, TrustTier
        MemoryStore(db_factory).create(Memory(
            memory_id="", user_id=uid,
            memory_type=MemoryType.PROCEDURAL,
            content="reflect test: read_file fails on symlinks, use realpath first",
            trust_tier=TrustTier.T3_INFERRED, session_id=sid,
        ))
        result = reflect_svc.build_evidence(sid, uid, "auto", 20)
        assert any("read_file fails on symlinks" in l for l in result["past_lessons"])
        assert any("Past lesson matches" in h for h in result["diagnosis_hints"])

    def test_feedback_signals_graceful_when_empty(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        result = reflect_svc.build_evidence(sid, uid, "auto", 20)
        assert result["feedback_signals"] == []

    def test_tool_selection_returns_usage_counts(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        result = reflect_svc.build_evidence(sid, uid, "tool_selection", 20)
        assert "tool_usage_counts" in result
        assert "read_file" in result["tool_usage_counts"]
        assert "bash" in result["tool_usage_counts"]
        assert "edge_tools" in result

    def test_tool_selection_question_hint(self, reflect_session, db_factory):
        sid, uid, _ = reflect_session
        from core.agent.reflect_service import ReflectService

        mock_skill = MagicMock()
        mock_skill.name = "list_prs"
        mock_skill.description = "List PRs"
        mock_skill.to_openai_schema.return_value = {
            "function": {"parameters": {"type": "object", "properties": {"repo": {"type": "string"}}}}
        }
        mock_reg = MagicMock()
        mock_reg._skills = {"list_prs": mock_skill}

        svc = ReflectService(db_factory=db_factory, skill_registry=mock_reg)
        result = svc.build_evidence(sid, uid, "tool_selection", 20, question="list_prs")
        assert any("list_prs" in h for h in result["diagnosis_hints"])

    def test_history_returns_related_queries(self, make_session, reflect_svc):
        uid = "reflect_hist_usr"
        make_session(uid, ["database connection pool exhausted yesterday"])
        cur_sid = make_session(uid, ["database connection pool exhausted again"])

        result = reflect_svc.build_evidence(cur_sid, uid, "history", 20)
        assert "related_history" in result
        assert len(result["related_history"]) >= 1

    def test_history_no_wildcard_injection(self, make_session, reflect_svc):
        uid = "reflect_esc_usr"
        make_session(uid, ["normal query about servers"])
        cur_sid = make_session(uid, ["100% match_test query"])

        result = reflect_svc.build_evidence(cur_sid, uid, "history", 20)
        matched_sids = {r["session_id"] for r in result["related_history"]}
        # Old session doesn't contain "100%" or "match_test" → must not match
        assert len(matched_sids) == 0

    def test_token_summary_present(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        ts = reflect_svc.build_evidence(sid, uid, "auto", 20)["token_summary"]
        assert ts["llm_calls"] == 0
        assert ts["total_tokens"] == 0
        assert ts["by_model"] == {}

    def test_token_summary_accumulates_llm_events(self, make_session, db_session, reflect_svc):
        uid = "reflect_tok_usr"
        sid = make_session(uid, ["hello"])

        from api.models.agent import Event as EventModel
        from core.events.event_logger import EventLogger
        for p, c in [(1000, 50), (2000, 100)]:
            db_session.add(EventModel(
                event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
                agent_id="test", event_type="llm_response", content=f"resp",
                causal_chain_id="chain", llm_model_used="test-model",
                token_usage=json.dumps({"prompt_tokens": p, "completion_tokens": c}),
            ))
        db_session.commit()

        ts = reflect_svc.build_evidence(sid, uid, "auto", 20)["token_summary"]
        assert ts["total_prompt_tokens"] == 3000
        assert ts["total_completion_tokens"] == 150
        assert ts["llm_calls"] == 2
        assert ts["by_model"]["test-model"]["calls"] == 2

    def test_tool_quality_summary_present(self, reflect_session, reflect_svc):
        sid, uid, _ = reflect_session
        assert reflect_svc.build_evidence(sid, uid, "auto", 20)["tool_quality_summary"] == []

    def test_tool_quality_summary_surfaces_degraded(self, make_session, db_session, reflect_svc):
        uid = "reflect_tq_usr"
        sid = make_session(uid, ["analyze stock"])

        from api.models.agent import Event as EventModel
        db_session.add(EventModel(
            event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
            agent_id="test", event_type="tool_result_quality", content="",
            causal_chain_id="chain",
            event_metadata={"tool_name": "stock_assistant", "quality_score": 0.35,
                            "quality_grade": "degraded", "missing_fields": ["technical_indicators"]},
        ))
        db_session.add(EventModel(
            event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
            agent_id="test", event_type="tool_result_quality", content="",
            causal_chain_id="chain",
            event_metadata={"tool_name": "bash", "quality_score": 1.0,
                            "quality_grade": "complete", "missing_fields": []},
        ))
        db_session.commit()

        tqs = reflect_svc.build_evidence(sid, uid, "auto", 20)["tool_quality_summary"]
        assert len(tqs) == 1
        assert tqs[0]["tool"] == "stock_assistant"
        assert tqs[0]["grade"] == "degraded"

    def test_high_token_usage_hint(self, make_session, db_session, reflect_svc):
        uid = "reflect_hightok"
        sid = make_session(uid, ["big query"])

        from api.models.agent import Event as EventModel
        db_session.add(EventModel(
            event_id=str(uuid.uuid4()), session_id=sid, user_id=uid,
            agent_id="test", event_type="llm_response", content="big",
            causal_chain_id="chain", llm_model_used="gpt-4",
            token_usage=json.dumps({"prompt_tokens": 60000, "completion_tokens": 2000}),
        ))
        db_session.commit()

        hints = reflect_svc.build_evidence(sid, uid, "auto", 20)["diagnosis_hints"]
        assert any("token" in h.lower() for h in hints)

    def test_cloud_skills_truncated_when_many(self, reflect_session, db_factory):
        """With 50 skills, only used + question-matched + 10 unused are returned in full."""
        sid, uid, _ = reflect_session
        from core.agent.reflect_service import ReflectService

        # Create 50 mock skills
        skills = {}
        for i in range(50):
            s = MagicMock()
            s.name = f"skill_{i:03d}"
            s.description = f"Skill number {i}"
            s.to_openai_schema.return_value = {
                "function": {"parameters": {"type": "object", "properties": {}}}
            }
            skills[s.name] = s
        mock_reg = MagicMock()
        mock_reg._skills = skills

        svc = ReflectService(db_factory=db_factory, skill_registry=mock_reg)
        result = svc.build_evidence(sid, uid, "tool_selection", 20)

        # Session used read_file and bash (from reflect_session fixture)
        # Those won't match mock skill names, so only 10 unused detail returned
        assert len(result["cloud_skills"]) <= 12  # 10 unused + up to 2 used
        assert result["cloud_skills_total"] == 50
        assert result.get("cloud_skills_omitted", 0) > 0

    def test_cloud_skills_question_match_included(self, reflect_session, db_factory):
        """Skills matching the question keyword are always included."""
        sid, uid, _ = reflect_session
        from core.agent.reflect_service import ReflectService

        skills = {}
        for i in range(30):
            s = MagicMock()
            s.name = f"skill_{i:03d}" if i != 15 else "deploy_service"
            s.description = f"Skill {i}" if i != 15 else "Deploy a service to production"
            s.to_openai_schema.return_value = {
                "function": {"parameters": {"type": "object", "properties": {}}}
            }
            skills[s.name] = s
        mock_reg = MagicMock()
        mock_reg._skills = skills

        svc = ReflectService(db_factory=db_factory, skill_registry=mock_reg)
        result = svc.build_evidence(sid, uid, "tool_selection", 20, question="deploy")

        names = [s["name"] for s in result["cloud_skills"]]
        assert "deploy_service" in names


# ============================================================================
# 3. Reflection learning — real DB Memory persistence
# ============================================================================

class TestReflectionLearningRealDB:

    @pytest.fixture(autouse=True)
    def _cleanup_lessons(self, reflect_session, db_session):
        yield
        _, uid, _ = reflect_session
        from api.models.memory import MemoryRecord
        db_session.query(MemoryRecord).filter(
            MemoryRecord.user_id == uid,
            MemoryRecord.content.like("%Reflection-driven%"),
        ).delete(synchronize_session=False)
        db_session.commit()

    def test_reflect_then_retry_creates_real_memory(self, reflect_session, db_factory):
        sid, uid, _ = reflect_session
        from core.agent.turn_hooks import TurnHooks
        from core.memory.store import MemoryStore
        from core.memory.types import MemoryType

        hooks = TurnHooks(db_factory)
        hooks.detect_reflection_learning(
            sid, uid,
            [{"function": {"name": "reflect"}}],
            [{"name": "reflect", "result": "read_file failed: file not found"}],
        )
        hooks.detect_reflection_learning(
            sid, uid,
            [{"function": {"name": "bash"}}],
            [{"name": "bash", "result": "ok"}],
        )

        store = MemoryStore(db_factory)
        memories = store.list_active(uid, MemoryType.PROCEDURAL)
        lessons = [m for m in memories if "Reflection-driven fix" in m.content]
        assert len(lessons) >= 1
        assert "bash" in lessons[0].content

    def test_no_lesson_without_reflect_first(self, reflect_session, db_factory):
        sid, uid, _ = reflect_session
        from core.agent.turn_hooks import TurnHooks
        from core.memory.store import MemoryStore
        from core.memory.types import MemoryType

        hooks = TurnHooks(db_factory)
        hooks.detect_reflection_learning(
            sid, uid,
            [{"function": {"name": "bash"}}],
            [{"name": "bash", "result": "ok"}],
        )

        store = MemoryStore(db_factory)
        memories = store.list_active(uid, MemoryType.PROCEDURAL)
        assert not any("Reflection-driven fix" in m.content for m in memories)


# ============================================================================
# 4. Local skill provenance
# ============================================================================

class TestLocalSkillProvenance:

    @pytest.mark.asyncio
    async def test_data_source_in_tool_result(self, _provenance_classes):
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
        from cli.tools.router import ToolRouter, ToolCall
        _, _, _, WithoutProvenance = _provenance_classes

        router = ToolRouter()
        router.register(WithoutProvenance())
        results = await router.execute([ToolCall(id="tc1", name="test_no_provenance", arguments={"query": "x"})])

        data = json.loads(results[0].result)
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

    def test_escapes_percent(self):
        from core.agent.reflect_service import _escape_like
        assert _escape_like("100%") == "100\\%"

    def test_escapes_underscore(self):
        from core.agent.reflect_service import _escape_like
        assert _escape_like("match_test") == "match\\_test"

    def test_escapes_backslash(self):
        from core.agent.reflect_service import _escape_like
        assert _escape_like("a\\b") == "a\\\\b"

    def test_plain_text_unchanged(self):
        from core.agent.reflect_service import _escape_like
        assert _escape_like("hello world") == "hello world"


class TestExplainSSE:

    @pytest.fixture
    def client(self):
        import os
        os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
        os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)
        from fastapi.testclient import TestClient
        from api.main import app
        return TestClient(app)

    def test_explain_event_in_stream(self, client, db_session):
        from unittest.mock import patch
        from tests.conftest import get_auth_headers, parse_sse_events, fake_llm_stream

        headers = get_auth_headers(client, db_session, username="explain_usr", user_id="explain_uid", email="ex@t.com")

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

    def test_explain_none_tokens_without_usage(self, client, db_session):
        from unittest.mock import patch
        from tests.conftest import get_auth_headers, parse_sse_events, fake_llm_stream

        headers = get_auth_headers(client, db_session, username="explain_nu", user_id="explain_nuid", email="en@t.com")

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
