"""Tests for session_analyzer, bare-word JSON repair, and cloud_loop_progress SSE handling.

Covers all new code added during the session debugging investigation:
1. SessionAnalyzer — timeline, gap detection, issue classification, recommendations
2. _try_repair_tool_args bare-word fix — "analysis_type": advice → "analysis_type": "advice"
3. edge_chat_loop cloud_loop_progress / cloud_tool_result event handling
4. ReflectService performance focus integration
"""

import json
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Any
from unittest.mock import MagicMock

import pytest


# ============================================================================
# 1. SessionAnalyzer tests
# ============================================================================

class TestSessionAnalyzer:
    """Test SessionAnalyzer with real DB events."""

    @pytest.fixture
    def analyzer(self, db_factory):
        from core.agent.session_analyzer import SessionAnalyzer
        return SessionAnalyzer(db_factory)

    @pytest.fixture
    def session_with_gaps(self, db_factory, db_session):
        """Create a session with deliberate time gaps to trigger gap detection."""
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger
        from sqlalchemy import text

        user_id = "analyzer_test_user"
        mgr = SessionManager(db_session)
        session = mgr.create_session(user_id=user_id)
        sid = session.session_id

        el = EventLogger(db_factory)
        base = datetime(2026, 3, 2, 14, 0, 0)

        # Create events via EventLogger (handles all required columns)
        events_spec = [
            ("user_query", "test question", None, base),
            ("tool_call", json.dumps({"name": "stock_assistant", "tool_call_id": "tc1"}),
             "stock_assistant", base + timedelta(seconds=1)),
            ("tool_result", json.dumps({"name": "stock_assistant", "result": "Malformed arguments JSON: bad"}),
             "stock_assistant", base + timedelta(seconds=8)),
            ("llm_response", "Here is the answer", None, base + timedelta(seconds=9)),
            # 50s gap — simulates cloud skill loop
            ("tool_call", json.dumps({"name": "execute_code", "arguments": {}, "source": "cloud"}),
             None, base + timedelta(seconds=59)),
            # Another 40s gap
            ("tool_call", json.dumps({"name": "execute_code", "arguments": {}, "source": "cloud"}),
             None, base + timedelta(seconds=99)),
            ("llm_response", "Final answer", None, base + timedelta(seconds=140)),
        ]

        event_ids = []
        uq = el.create_user_query(user_id=user_id, session_id=sid, content="test question")
        event_ids.append((uq.event_id, base))
        chain = uq.causal_chain_id

        for etype, content, skill, ts in events_spec[1:]:
            if etype == "llm_response":
                ev = el.create_llm_response(
                    user_id=user_id, session_id=sid, content=content,
                    agent_id="dev-agent", agent_version="0.1.0",
                    parent_event_id=uq.event_id, causal_chain_id=chain,
                )
            else:
                ev = el.create_stream_event(
                    user_id=user_id, session_id=sid, event_type=etype,
                    content=content, parent_event_id=uq.event_id,
                    causal_chain_id=chain, skill_name=skill,
                )
            event_ids.append((ev.event_id, ts))

        # Backfill created_at to simulate time gaps
        with db_factory() as db:
            for eid, ts in event_ids:
                db.execute(text(
                    "UPDATE agent_events SET created_at = :ts WHERE event_id = :eid"
                ), {"ts": ts, "eid": eid})
            db.commit()

        yield sid, user_id

        # Cleanup
        from api.models.agent import Event as EventModel, Session as SessionModel
        db_session.query(EventModel).filter(EventModel.session_id == sid).delete()
        db_session.query(SessionModel).filter(SessionModel.session_id == sid).delete()
        db_session.commit()

    def test_empty_session(self, analyzer):
        report = analyzer.analyze("nonexistent-session-id")
        assert report.total_duration_s == 0
        assert len(report.issues) == 1
        assert report.issues[0]["type"] == "empty"

    def test_gap_detection(self, analyzer, session_with_gaps):
        sid, _ = session_with_gaps
        report = analyzer.analyze(sid)

        slow_gaps = [i for i in report.issues if i["type"] == "slow_gap"]
        assert len(slow_gaps) >= 2, f"Expected >=2 slow gaps, got {slow_gaps}"
        # The 50s and 40s gaps should be detected
        gap_values = sorted([i["gap_s"] for i in slow_gaps], reverse=True)
        assert gap_values[0] >= 40

    def test_tool_error_detection(self, analyzer, session_with_gaps):
        sid, _ = session_with_gaps
        report = analyzer.analyze(sid)

        tool_errors = [i for i in report.issues if i["type"] == "tool_error"]
        assert len(tool_errors) == 1
        assert "Malformed" in tool_errors[0]["description"]

    def test_cloud_loop_storm(self, analyzer, session_with_gaps):
        sid, _ = session_with_gaps
        report = analyzer.analyze(sid)

        # 2 cloud execute_code calls — below threshold of 3, so no storm
        # But stats should track them
        assert report.stats["cloud_skill_calls"] == 2

    def test_timeline_order(self, analyzer, session_with_gaps):
        sid, _ = session_with_gaps
        report = analyzer.analyze(sid)

        assert len(report.timeline) == 7
        # Verify chronological order
        for i in range(1, len(report.timeline)):
            assert report.timeline[i].ts >= report.timeline[i - 1].ts

    def test_to_markdown(self, analyzer, session_with_gaps):
        sid, _ = session_with_gaps
        report = analyzer.analyze(sid)
        md = report.to_markdown()

        assert "## Session Analysis" in md
        assert "### Timeline" in md
        assert "### Issues Found" in md
        assert "⚠️" in md  # Gap markers

    def test_to_dict(self, analyzer, session_with_gaps):
        sid, _ = session_with_gaps
        report = analyzer.analyze(sid)
        d = report.to_dict()

        assert d["session_id"] == sid
        assert isinstance(d["timeline"], list)
        assert isinstance(d["issues"], list)
        assert isinstance(d["recommendations"], list)
        assert "total_events" in d["stats"]

    def test_recommendations_for_large_gap(self, analyzer, session_with_gaps):
        sid, _ = session_with_gaps
        report = analyzer.analyze(sid)

        # Should recommend model routing for large gaps
        has_latency_rec = any("latency" in r.lower() or "model" in r.lower()
                             for r in report.recommendations)
        has_error_rec = any("error" in r.lower() for r in report.recommendations)
        assert has_latency_rec or has_error_rec

    def test_summarize_event_types(self, analyzer):
        from core.agent.session_analyzer import SessionAnalyzer
        assert SessionAnalyzer._summarize_event("user_query", "hello world", None) == "hello world"
        assert SessionAnalyzer._summarize_event("session_history_snapshot", None, None) == "snapshot"
        assert "→" in SessionAnalyzer._summarize_event(
            "tool_call", json.dumps({"name": "bash"}), "bash")
        assert "←" in SessionAnalyzer._summarize_event(
            "tool_result", json.dumps({"name": "bash", "result": "ok"}), "bash")


# ============================================================================
# 2. _try_repair_tool_args bare-word fix
# ============================================================================

class TestTryRepairToolArgs:
    """Test the bare-word JSON value repair in _try_repair_tool_args."""

    @pytest.fixture
    def repair_fn(self):
        from api.routers.chat import _try_repair_tool_args
        return _try_repair_tool_args

    def test_bare_word_value(self, repair_fn):
        """The exact bug from session 019caedc: "analysis_type": advice"""
        raw = '{"query": "test", "analysis_type": advice, "period": "1mo"}'
        result = repair_fn("stock_assistant", raw)
        assert result is not None
        assert result["analysis_type"] == "advice"
        assert result["period"] == "1mo"

    def test_preserves_json_true(self, repair_fn):
        raw = '{"flag": true, "name": hello}'
        result = repair_fn("test", raw)
        assert result is not None
        assert result["flag"] is True
        assert result["name"] == "hello"

    def test_preserves_json_false(self, repair_fn):
        raw = '{"active": false}'
        result = repair_fn("test", raw)
        assert result is not None
        assert result["active"] is False

    def test_preserves_json_null(self, repair_fn):
        raw = '{"data": null}'
        result = repair_fn("test", raw)
        assert result is not None
        assert result["data"] is None

    def test_valid_json_unchanged(self, repair_fn):
        raw = '{"key": "value", "num": 42}'
        result = repair_fn("test", raw)
        assert result is not None
        assert result["key"] == "value"
        assert result["num"] == 42

    def test_trailing_comma(self, repair_fn):
        raw = '{"key": "value",}'
        result = repair_fn("test", raw)
        assert result is not None
        assert result["key"] == "value"

    def test_empty_string(self, repair_fn):
        result = repair_fn("test", "")
        assert result is None

    def test_multiple_bare_words(self, repair_fn):
        raw = '{"type": advice, "mode": fast, "count": 3}'
        result = repair_fn("test", raw)
        assert result is not None
        assert result["type"] == "advice"
        assert result["mode"] == "fast"
        assert result["count"] == 3


# ============================================================================
# 3. edge_chat_loop cloud event handling
# ============================================================================

class TestCloudLoopProgressEvents:
    """Test that _consume_turn handles cloud_loop_progress and cloud_tool_result."""

    @dataclass
    class RecordingRenderer:
        texts: list[str] = field(default_factory=list)
        tool_starts: list[str] = field(default_factory=list)
        tool_dones: list[tuple[str, bool]] = field(default_factory=list)
        errors: list[str] = field(default_factory=list)
        infos: list[str] = field(default_factory=list)
        thinking_msgs: list[str] = field(default_factory=list)

        def text(self, chunk: str) -> None:
            self.texts.append(chunk)
        def tool_start(self, name: str, args: dict[str, Any]) -> None:
            self.tool_starts.append(name)
        def tool_done(self, name: str, result: str, error: bool) -> None:
            self.tool_dones.append((name, error))
        def error(self, msg: str) -> None:
            self.errors.append(msg)
        def info(self, msg: str) -> None:
            self.infos.append(msg)
        def thinking(self, msg: str = "") -> None:
            self.thinking_msgs.append(msg)
        def thinking_hide(self) -> None:
            pass

    @pytest.mark.asyncio
    async def test_cloud_loop_progress_triggers_thinking(self):
        from cli.edge_chat_loop import _consume_turn

        async def fake_stream():
            yield {"type": "cloud_loop_progress", "loop": 0, "cloud_skills": 1, "edge_skills": 0}
            yield {"type": "cloud_tool_result", "name": "execute_code", "result": "ok"}
            yield {"type": "text_delta", "content": "Done"}
            yield {"type": "turn_complete", "has_tool_calls": False}

        renderer = self.RecordingRenderer()
        result = await _consume_turn(fake_stream(), renderer)

        assert result.text == "Done"
        # thinking() should have been called for both cloud events
        assert len(renderer.thinking_msgs) >= 2
        assert any("cloud skill" in m.lower() or "step" in m.lower()
                    for m in renderer.thinking_msgs)

    @pytest.mark.asyncio
    async def test_cloud_events_without_thinking_method(self):
        """Renderer without thinking() should not crash."""
        from cli.edge_chat_loop import _consume_turn

        async def fake_stream():
            yield {"type": "cloud_loop_progress", "loop": 0, "cloud_skills": 1}
            yield {"type": "cloud_tool_result", "name": "test", "result": "ok"}
            yield {"type": "text_delta", "content": "Answer"}
            yield {"type": "turn_complete", "has_tool_calls": False}

        @dataclass
        class MinimalRenderer:
            texts: list[str] = field(default_factory=list)
            def text(self, chunk: str) -> None:
                self.texts.append(chunk)
            def tool_start(self, name: str, args: dict) -> None: pass
            def tool_done(self, name: str, result: str, error: bool) -> None: pass
            def error(self, msg: str) -> None: pass
            def info(self, msg: str) -> None: pass

        renderer = MinimalRenderer()
        result = await _consume_turn(fake_stream(), renderer)
        assert result.text == "Answer"
        assert not result.has_tool_calls


# ============================================================================
# 4. ReflectService performance focus
# ============================================================================

class TestReflectPerformanceFocus:
    """Test that performance focus returns session_report."""

    @pytest.fixture
    def simple_session(self, db_factory, db_session):
        """Minimal session for reflect tests."""
        from core.events.session_manager import SessionManager
        from core.events.event_logger import EventLogger

        user_id = "perf_focus_user"
        mgr = SessionManager(db_session)
        session = mgr.create_session(user_id=user_id)
        sid = session.session_id

        el = EventLogger(db_factory)
        el.create_user_query(user_id=user_id, session_id=sid, content="hello")

        yield sid, user_id

        from api.models.agent import Event as EventModel, Session as SessionModel
        db_session.query(EventModel).filter(EventModel.session_id == sid).delete()
        db_session.query(SessionModel).filter(SessionModel.session_id == sid).delete()
        db_session.commit()

    def test_performance_focus_returns_report(self, simple_session, db_factory):
        from core.agent.reflect_service import ReflectService
        sid, user_id = simple_session

        svc = ReflectService(db_factory=db_factory)
        result = svc.build_evidence(sid, user_id, "performance", 20)

        assert "session_report" in result
        assert "session_report_markdown" in result
        assert result["session_report"]["session_id"] == sid
        assert isinstance(result["session_report"]["timeline"], list)
        assert isinstance(result["session_report"]["issues"], list)

    def test_performance_markdown_has_structure(self, simple_session, db_factory):
        from core.agent.reflect_service import ReflectService
        sid, user_id = simple_session

        svc = ReflectService(db_factory=db_factory)
        result = svc.build_evidence(sid, user_id, "performance", 20)

        md = result["session_report_markdown"]
        assert "## Session Analysis" in md
        assert "### Timeline" in md
        assert "### Stats" in md

    def test_auto_focus_also_includes_report(self, simple_session, db_factory):
        from core.agent.reflect_service import ReflectService
        sid, user_id = simple_session

        svc = ReflectService(db_factory=db_factory)
        result = svc.build_evidence(sid, user_id, "auto", 20)

        assert "session_report" in result

    def test_non_performance_focus_excludes_report(self, simple_session, db_factory):
        from core.agent.reflect_service import ReflectService
        sid, user_id = simple_session

        svc = ReflectService(db_factory=db_factory)
        result = svc.build_evidence(sid, user_id, "history", 20)

        assert "session_report" not in result
