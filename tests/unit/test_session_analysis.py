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

    def test_user_query_gap_not_flagged_as_issue(self):
        """Gap before user_query is user think time — must NOT be reported as slow_gap."""
        from core.agent.session_analyzer import SessionAnalyzer
        from unittest.mock import patch, MagicMock

        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            # First turn
            ("e1", "user_query", "first question", None, t0, None, {}, None, None),
            ("e2", "llm_response", "answer 1", None, t0 + timedelta(seconds=2), None, {}, None, None),
            # 445s user think time before second user_query — must NOT be flagged
            ("e3", "user_query", "second question", None, t0 + timedelta(seconds=447), None, {}, None, None),
            ("e4", "llm_response", "answer 2", None, t0 + timedelta(seconds=449), None, {}, None, None),
        ]

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_result = MagicMock()
        mock_result.fetchall.return_value = rows
        mock_db.execute.return_value = mock_result

        analyzer = SessionAnalyzer(db_factory=lambda: mock_db)
        report = analyzer.analyze("test-session")

        slow_gaps = [i for i in report.issues if i["type"] == "slow_gap"]
        assert len(slow_gaps) == 0, (
            f"user_query gap should not be flagged as slow_gap, but got: {slow_gaps}"
        )

    def test_summarize_event_types(self, analyzer):
        from core.agent.session_analyzer import SessionAnalyzer
        assert SessionAnalyzer._summarize_event("user_query", "hello world", None) == "hello world"
        assert SessionAnalyzer._summarize_event("session_history_snapshot", None, None) == "snapshot"
        assert "→" in SessionAnalyzer._summarize_event(
            "tool_call", json.dumps({"name": "bash"}), "bash")
        assert "←" in SessionAnalyzer._summarize_event(
            "tool_result", json.dumps({"name": "bash", "result": "ok"}), "bash")

    def test_stall_detection_in_analyzer(self):
        """Analyzer detects repeated identical tool calls as a stall pattern.

        Simulates realistic event ordering with llm_response between each
        tool-call round (as happens in production):
          user_query → llm_response → tc,tr → llm_response → tc,tr → …
        The analyzer must still detect the stall across llm_response boundaries.
        """
        from core.agent.session_analyzer import SessionAnalyzer

        t0 = datetime(2026, 1, 1, 0, 0, 0)
        # Build realistic ordering: llm_response → tool_call → tool_result per round
        events = []
        for i in range(5):
            base = i * 3 + 1
            events.append(
                (f"r{i}", "llm_response", "Checking...", None,
                 t0 + timedelta(seconds=base), None, {}, None, None))
            events.append(
                (f"tc{i}", "tool_call",
                 json.dumps({"name": "git_status", "arguments": "{}"}),
                 None, t0 + timedelta(seconds=base + 1), None, {}, None, None))
            events.append(
                (f"tr{i}", "tool_result",
                 json.dumps({"name": "git_status", "result": "M file.py"}),
                 None, t0 + timedelta(seconds=base + 2), None, {}, None, None))
        rows = [
            ("e0", "user_query", "check status", None, t0, None, {}, None, None),
            *events,
            ("e_final", "llm_response", "Here is the answer", None,
             t0 + timedelta(seconds=20), None, {}, None, None),
        ]

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_result = MagicMock()
        mock_result.fetchall.return_value = rows
        mock_db.execute.return_value = mock_result

        analyzer = SessionAnalyzer(db_factory=lambda: mock_db)
        report = analyzer.analyze("stall-session")

        stall_issues = [i for i in report.issues if i["type"] == "tool_call_stall"]
        assert len(stall_issues) == 1
        assert stall_issues[0]["repeat_count"] == 5
        assert "git_status" in stall_issues[0]["description"]

        stall_recs = [r for r in report.recommendations if "stall" in r.lower()]
        assert len(stall_recs) == 1

    def test_no_stall_with_different_args(self):
        """Different arguments each call → no stall detected."""
        from core.agent.session_analyzer import SessionAnalyzer

        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            ("e0", "user_query", "explore", None, t0, None, {}, None, None),
            ("tc1", "tool_call",
             json.dumps({"name": "read_file", "arguments": '{"path":"a.py"}'}),
             None, t0 + timedelta(seconds=1), None, {}, None, None),
            ("tc2", "tool_call",
             json.dumps({"name": "read_file", "arguments": '{"path":"b.py"}'}),
             None, t0 + timedelta(seconds=2), None, {}, None, None),
            ("tc3", "tool_call",
             json.dumps({"name": "read_file", "arguments": '{"path":"c.py"}'}),
             None, t0 + timedelta(seconds=3), None, {}, None, None),
            ("e_final", "llm_response", "Done", None,
             t0 + timedelta(seconds=4), None, {}, None, None),
        ]

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_result = MagicMock()
        mock_result.fetchall.return_value = rows
        mock_db.execute.return_value = mock_result

        analyzer = SessionAnalyzer(db_factory=lambda: mock_db)
        report = analyzer.analyze("no-stall-session")

        stall_issues = [i for i in report.issues if i["type"] == "tool_call_stall"]
        assert len(stall_issues) == 0

    def test_stall_spans_llm_response_boundary(self):
        """Identical tool calls across llm_response boundary → stall detected.

        llm_response does NOT reset stall tracking (only user_query does).
        In a real stall the sequence is: llm_response → tc → tr → llm_response → tc → …
        so resetting on llm_response would prevent detection entirely.
        Here 4 identical calls (2 before + 2 after llm_response) exceed threshold=3.
        """
        from core.agent.session_analyzer import SessionAnalyzer

        t0 = datetime(2026, 1, 1, 0, 0, 0)
        tc = json.dumps({"name": "git_status", "arguments": "{}"})
        tr = json.dumps({"name": "git_status", "result": "M file.py"})
        rows = [
            ("e0", "user_query", "check", None, t0, None, {}, None, None),
            # Round 1: 2 identical calls
            ("r0", "llm_response", "Let me check...", None, t0 + timedelta(seconds=1), None, {}, None, None),
            ("tc1", "tool_call", tc, None, t0 + timedelta(seconds=2), None, {}, None, None),
            ("tr1", "tool_result", tr, None, t0 + timedelta(seconds=3), None, {}, None, None),
            ("r1", "llm_response", "Checking again...", None, t0 + timedelta(seconds=4), None, {}, None, None),
            ("tc2", "tool_call", tc, None, t0 + timedelta(seconds=5), None, {}, None, None),
            ("tr2", "tool_result", tr, None, t0 + timedelta(seconds=6), None, {}, None, None),
            # llm_response between rounds — must NOT reset stall tracking
            ("r2", "llm_response", "One more time...", None, t0 + timedelta(seconds=7), None, {}, None, None),
            # Round 2: 2 more identical calls → total 4 ≥ threshold 3
            ("tc3", "tool_call", tc, None, t0 + timedelta(seconds=8), None, {}, None, None),
            ("tr3", "tool_result", tr, None, t0 + timedelta(seconds=9), None, {}, None, None),
            ("r3", "llm_response", "And again...", None, t0 + timedelta(seconds=10), None, {}, None, None),
            ("tc4", "tool_call", tc, None, t0 + timedelta(seconds=11), None, {}, None, None),
            ("tr4", "tool_result", tr, None, t0 + timedelta(seconds=12), None, {}, None, None),
            ("e_final", "llm_response", "Done", None,
             t0 + timedelta(seconds=13), None, {}, None, None),
        ]

        mock_db = MagicMock()
        mock_db.__enter__ = MagicMock(return_value=mock_db)
        mock_db.__exit__ = MagicMock(return_value=False)
        mock_result = MagicMock()
        mock_result.fetchall.return_value = rows
        mock_db.execute.return_value = mock_result

        analyzer = SessionAnalyzer(db_factory=lambda: mock_db)
        report = analyzer.analyze("stall-across-llm-response")

        stall_issues = [i for i in report.issues if i["type"] == "tool_call_stall"]
        assert len(stall_issues) == 1, \
            "4 identical calls across llm_response boundary should be detected as stall"
        assert stall_issues[0]["repeat_count"] == 4


# ============================================================================
# 1b. Execution tree, summary, cost, and ASCII rendering (unit tests)
# ============================================================================

class TestCalculateCost:
    """Unit tests for _calculate_cost."""

    def test_known_model(self):
        from core.agent.session_analyzer import _calculate_cost
        cost = _calculate_cost("gpt-4o", 1_000_000, 1_000_000)
        assert cost == pytest.approx(2.50 + 10.00)

    def test_prefix_match(self):
        from core.agent.session_analyzer import _calculate_cost
        cost = _calculate_cost("gpt-4o-2024-08-06", 1_000_000, 0)
        assert cost == pytest.approx(2.50)

    def test_unknown_model_returns_zero(self):
        from core.agent.session_analyzer import _calculate_cost
        cost = _calculate_cost("unknown-model-xyz", 1000, 1000)
        assert cost == 0.0

    def test_zero_tokens(self):
        from core.agent.session_analyzer import _calculate_cost
        assert _calculate_cost("gpt-4o", 0, 0) == 0.0


class TestExecutionNodeAscii:
    """Unit tests for ExecutionNode.to_ascii rendering."""

    def test_single_node(self):
        from core.agent.session_analyzer import ExecutionNode
        node = ExecutionNode(
            node_id="1", node_type="user_query", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=5.0, detail="hello",
        )
        lines = node.to_ascii()
        assert len(lines) == 1
        assert "user_query" in lines[0]
        assert "hello" in lines[0]
        assert "5.00s" in lines[0]

    def test_parent_duration_pct_shown(self):
        from core.agent.session_analyzer import ExecutionNode
        node = ExecutionNode(
            node_id="1", node_type="llm_response", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=3.0,
            parent_duration_pct=75.0,
        )
        lines = node.to_ascii()
        assert "75%" in lines[0]

    def test_parent_duration_pct_zero_shown(self):
        """parent_duration_pct=0.0 should still render (not be skipped by truthiness)."""
        from core.agent.session_analyzer import ExecutionNode
        node = ExecutionNode(
            node_id="1", node_type="llm_response", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            parent_duration_pct=0.0,
        )
        lines = node.to_ascii()
        assert "0%" in lines[0]

    def test_tokens_displayed(self):
        from core.agent.session_analyzer import ExecutionNode
        node = ExecutionNode(
            node_id="1", node_type="llm_response", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            tokens_in=100, tokens_out=50,
        )
        lines = node.to_ascii()
        assert "100→50 tokens" in lines[0]

    def test_cost_displayed(self):
        from core.agent.session_analyzer import ExecutionNode
        node = ExecutionNode(
            node_id="1", node_type="llm_response", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            cost_usd=0.0123,
        )
        lines = node.to_ascii()
        assert "$0.0123" in lines[0]

    def test_issues_displayed(self):
        from core.agent.session_analyzer import ExecutionNode
        node = ExecutionNode(
            node_id="1", node_type="tool_result", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=15.0,
            issues=["SLOW", "BOTTLENECK"],
        )
        lines = node.to_ascii()
        assert "SLOW" in lines[0]
        assert "BOTTLENECK" in lines[0]

    def test_children_rendered(self):
        from core.agent.session_analyzer import ExecutionNode
        child = ExecutionNode(
            node_id="c1", node_type="tool_call", event_id="c1",
            ts=datetime(2026, 1, 1), duration_s=0,
            detail="bash",
        )
        parent = ExecutionNode(
            node_id="p1", node_type="llm_response", event_id="p1",
            ts=datetime(2026, 1, 1), duration_s=2.0,
            children=[child],
        )
        lines = parent.to_ascii()
        assert len(lines) == 2
        assert "└─" in lines[1]
        assert "bash" in lines[1]

    def test_tool_result_metadata_rendered(self):
        from core.agent.session_analyzer import ExecutionNode
        node = ExecutionNode(
            node_id="1", node_type="tool_result", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            metadata={"api_latency_ms": 500, "result_size_bytes": 2048, "result_size_tokens": 300},
        )
        lines = node.to_ascii()
        assert any("api_latency: 500ms" in l for l in lines)
        assert any("result_size: 2.0KB" in l for l in lines)
        assert any("tokens_added: 300" in l for l in lines)


class TestBuildExecutionTree:
    """Unit tests for _build_execution_tree and _build_basic_tree."""

    def _make_row(self, event_id, event_type, content, skill_name, ts, agent_id="agent", metadata=None,
                  token_usage=None, llm_model=None):
        """Create a fake DB row tuple matching the SELECT column order (9 columns)."""
        return (event_id, event_type, content, skill_name, ts, agent_id, metadata or {},
                token_usage, llm_model)

    def _make_analyzer(self):
        from core.agent.session_analyzer import SessionAnalyzer
        # SessionAnalyzer needs a db_factory but tree building doesn't use DB
        return SessionAnalyzer(db_factory=lambda: None)

    def test_empty_rows(self):
        analyzer = self._make_analyzer()
        tree = analyzer._build_execution_tree([])
        assert tree.node_type == "empty"

    def test_single_user_query(self):
        analyzer = self._make_analyzer()
        rows = [self._make_row("e1", "user_query", "hello", None, datetime(2026, 1, 1, 0, 0, 0))]
        tree = analyzer._build_execution_tree(rows)
        assert tree.node_type == "session"
        assert len(tree.children) == 1
        assert tree.children[0].node_type == "user_query"

    def test_simple_turn(self):
        """user_query → llm_response should produce correct parent-child."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "hello", None, t0),
            self._make_row("e2", "llm_response", "world", None, t0 + timedelta(seconds=2)),
        ]
        tree = analyzer._build_execution_tree(rows)
        uq = tree.children[0]
        assert uq.node_type == "user_query"
        assert len(uq.children) == 1
        llm = uq.children[0]
        assert llm.node_type == "llm_response"
        assert llm.duration_s == pytest.approx(2.0)

    def test_multi_turn_tool_use(self):
        """user_query → llm → tool_call → tool_result → llm (turn 2)."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "do stuff", None, t0),
            self._make_row("e2", "llm_response", "calling tool", None, t0 + timedelta(seconds=1)),
            self._make_row("e3", "tool_call", json.dumps({"name": "bash"}), "bash", t0 + timedelta(seconds=2)),
            self._make_row("e4", "tool_result", json.dumps({"name": "bash", "result": "ok"}), "bash", t0 + timedelta(seconds=5)),
            self._make_row("e5", "llm_response", "done", None, t0 + timedelta(seconds=7)),
        ]
        tree = analyzer._build_execution_tree(rows)
        uq = tree.children[0]
        assert uq.node_type == "user_query"
        # Should have 2 llm_response children (turn 1 and turn 2)
        llm_children = [c for c in uq.children if c.node_type == "llm_response"]
        assert len(llm_children) == 2, f"Expected 2 llm_responses, got {len(llm_children)}"

        # First llm_response should have tool_call child
        llm1 = llm_children[0]
        tool_calls = [c for c in llm1.children if c.node_type == "tool_call"]
        assert len(tool_calls) == 1
        assert tool_calls[0].detail == "bash"

        # tool_call should have tool_result child
        assert len(tool_calls[0].children) == 1
        assert tool_calls[0].children[0].node_type == "tool_result"
        assert tool_calls[0].children[0].duration_s == pytest.approx(3.0)

    def test_multiple_tool_calls_in_one_turn(self):
        """llm_response with 2 tool_calls, each matched to its result."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row("e2", "llm_response", "r", None, t0 + timedelta(seconds=1)),
            self._make_row("e3", "tool_call", json.dumps({"name": "foo"}), "foo", t0 + timedelta(seconds=2)),
            self._make_row("e4", "tool_call", json.dumps({"name": "bar"}), "bar", t0 + timedelta(seconds=3)),
            self._make_row("e5", "tool_result", json.dumps({"name": "foo", "result": "ok"}), "foo", t0 + timedelta(seconds=5)),
            self._make_row("e6", "tool_result", json.dumps({"name": "bar", "result": "ok"}), "bar", t0 + timedelta(seconds=6)),
        ]
        tree = analyzer._build_execution_tree(rows)
        llm = tree.children[0].children[0]
        assert len(llm.children) == 2
        assert llm.children[0].detail == "foo"
        assert llm.children[1].detail == "bar"
        # Each tool_call should have exactly one tool_result child
        assert len(llm.children[0].children) == 1
        assert len(llm.children[1].children) == 1
        assert llm.children[0].children[0].detail == "foo"
        assert llm.children[1].children[0].detail == "bar"

    def test_tool_call_duration_is_zero(self):
        """tool_call node itself should have duration=0 (not fake 0.05s)."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row("e2", "llm_response", "r", None, t0 + timedelta(seconds=1)),
            self._make_row("e3", "tool_call", json.dumps({"name": "x"}), "x", t0 + timedelta(seconds=2)),
            self._make_row("e4", "tool_result", json.dumps({"name": "x", "result": "ok"}), "x", t0 + timedelta(seconds=4)),
        ]
        tree = analyzer._build_execution_tree(rows)
        llm = tree.children[0].children[0]
        tc = llm.children[0]
        assert tc.node_type == "tool_call"
        assert tc.duration_s == 0

    def test_malformed_content_no_crash(self):
        """Malformed JSON in tool_call/tool_result should not crash."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row("e2", "llm_response", "r", None, t0 + timedelta(seconds=1)),
            self._make_row("e3", "tool_call", "not json", "fallback_skill", t0 + timedelta(seconds=2)),
            self._make_row("e4", "tool_result", "also not json", "fallback_skill", t0 + timedelta(seconds=3)),
        ]
        tree = analyzer._build_execution_tree(rows)
        # Should not raise; tool_call should use skill_name as fallback
        llm = tree.children[0].children[0]
        assert llm.children[0].detail == "fallback_skill"

    def test_orphan_tool_result_attached(self):
        """tool_result with no matching tool_call should still appear in tree."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row("e2", "llm_response", "r", None, t0 + timedelta(seconds=1)),
            self._make_row("e3", "tool_result", json.dumps({"name": "orphan", "result": "ok"}), "orphan", t0 + timedelta(seconds=2)),
        ]
        tree = analyzer._build_execution_tree(rows)
        llm = tree.children[0].children[0]
        # Orphan should be attached directly to llm_response
        assert any(c.node_type == "tool_result" and c.detail == "orphan" for c in llm.children)

    def test_other_event_types_attached(self):
        """Non-standard event types (e.g. system_message) should appear as leaves."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row("e2", "system_message", "sys msg", None, t0 + timedelta(seconds=1)),
        ]
        tree = analyzer._build_execution_tree(rows)
        uq = tree.children[0]
        assert any(c.node_type == "system_message" for c in uq.children)

    def test_user_query_duration_spans_descendants(self):
        """user_query duration should span from its ts to its last descendant."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row("e2", "llm_response", "r", None, t0 + timedelta(seconds=5)),
            self._make_row("e3", "tool_call", json.dumps({"name": "x"}), "x", t0 + timedelta(seconds=6)),
            self._make_row("e4", "tool_result", json.dumps({"name": "x", "result": "ok"}), "x", t0 + timedelta(seconds=20)),
        ]
        tree = analyzer._build_execution_tree(rows)
        uq = tree.children[0]
        assert uq.duration_s == pytest.approx(20.0)

    def test_token_usage_and_model_merged_into_tree(self):
        """token_usage and llm_model_used columns should populate ExecutionNode metrics."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row(
                "e2", "llm_response", "r", None, t0 + timedelta(seconds=2),
                token_usage={"prompt_tokens": 1000, "completion_tokens": 200},
                llm_model="gpt-4o",
            ),
        ]
        tree = analyzer._build_execution_tree(rows)
        llm = tree.children[0].children[0]
        assert llm.tokens_in == 1000
        assert llm.tokens_out == 200
        assert llm.cost_usd is not None and llm.cost_usd > 0

    def test_tool_result_metadata_from_row(self):
        """Tool result metadata (duration_ms, result_size_bytes) should be available."""
        analyzer = self._make_analyzer()
        t0 = datetime(2026, 1, 1, 0, 0, 0)
        rows = [
            self._make_row("e1", "user_query", "q", None, t0),
            self._make_row("e2", "llm_response", "r", None, t0 + timedelta(seconds=1)),
            self._make_row("e3", "tool_call", json.dumps({"name": "bash"}), "bash", t0 + timedelta(seconds=2)),
            self._make_row(
                "e4", "tool_result", json.dumps({"name": "bash", "result": "ok"}), "bash",
                t0 + timedelta(seconds=5),
                metadata={"duration_ms": 3000, "result_size_bytes": 4096, "result_size_tokens": 500},
            ),
        ]
        tree = analyzer._build_execution_tree(rows)
        llm = tree.children[0].children[0]
        tc = llm.children[0]
        tr = tc.children[0]
        assert tr.metadata["duration_ms"] == 3000
        assert tr.metadata["result_size_bytes"] == 4096


class TestCalculateMetrics:
    """Unit tests for _calculate_metrics issue detection."""

    def _make_analyzer(self):
        from core.agent.session_analyzer import SessionAnalyzer
        return SessionAnalyzer(db_factory=lambda: None)

    def test_slow_detection(self):
        from core.agent.session_analyzer import ExecutionNode, SLOW_NODE_THRESHOLD_S
        analyzer = self._make_analyzer()
        node = ExecutionNode(
            node_id="1", node_type="tool_result", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=SLOW_NODE_THRESHOLD_S,
        )
        analyzer._calculate_metrics(node, None)
        assert "SLOW" in node.issues

    def test_not_slow_below_threshold(self):
        from core.agent.session_analyzer import ExecutionNode, SLOW_NODE_THRESHOLD_S
        analyzer = self._make_analyzer()
        node = ExecutionNode(
            node_id="1", node_type="tool_result", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=SLOW_NODE_THRESHOLD_S - 1,
        )
        analyzer._calculate_metrics(node, None)
        assert "SLOW" not in node.issues

    def test_bottleneck_detection(self):
        from core.agent.session_analyzer import ExecutionNode
        analyzer = self._make_analyzer()
        parent = ExecutionNode(
            node_id="p", node_type="user_query", event_id="p",
            ts=datetime(2026, 1, 1), duration_s=10.0,
        )
        child = ExecutionNode(
            node_id="c", node_type="llm_response", event_id="c",
            ts=datetime(2026, 1, 1), duration_s=8.0,
        )
        analyzer._calculate_metrics(child, parent)
        assert "BOTTLENECK" in child.issues
        assert child.parent_duration_pct == pytest.approx(80.0)

    def test_high_token_detection(self):
        from core.agent.session_analyzer import ExecutionNode, HIGH_TOKEN_THRESHOLD
        analyzer = self._make_analyzer()
        node = ExecutionNode(
            node_id="1", node_type="llm_response", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            tokens_in=HIGH_TOKEN_THRESHOLD + 1,
        )
        analyzer._calculate_metrics(node, None)
        assert "HIGH_TOKEN" in node.issues

    def test_expensive_detection(self):
        from core.agent.session_analyzer import ExecutionNode, EXPENSIVE_THRESHOLD_USD
        analyzer = self._make_analyzer()
        node = ExecutionNode(
            node_id="1", node_type="llm_response", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            cost_usd=EXPENSIVE_THRESHOLD_USD + 0.001,
        )
        analyzer._calculate_metrics(node, None)
        assert "EXPENSIVE" in node.issues

    def test_large_context_detection(self):
        from core.agent.session_analyzer import ExecutionNode, LARGE_CONTEXT_THRESHOLD
        analyzer = self._make_analyzer()
        node = ExecutionNode(
            node_id="1", node_type="tool_result", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            metadata={"result_size_tokens": LARGE_CONTEXT_THRESHOLD + 1},
        )
        analyzer._calculate_metrics(node, None)
        assert "LARGE_CONTEXT" in node.issues

    def test_no_issues_on_clean_node(self):
        from core.agent.session_analyzer import ExecutionNode
        analyzer = self._make_analyzer()
        node = ExecutionNode(
            node_id="1", node_type="llm_response", event_id="1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            tokens_in=100, cost_usd=0.001,
        )
        analyzer._calculate_metrics(node, None)
        assert node.issues == []


class TestBuildSummary:
    """Unit tests for _build_summary."""

    def _make_analyzer(self):
        from core.agent.session_analyzer import SessionAnalyzer
        return SessionAnalyzer(db_factory=lambda: None)

    def test_self_time_accounting(self):
        """Time uses 'self time' (node - children), not leaf-only."""
        from core.agent.session_analyzer import ExecutionNode
        analyzer = self._make_analyzer()

        # llm_response: 10s total, child tool_result: 8s → self time = 2s
        child = ExecutionNode(
            node_id="c", node_type="tool_result", event_id="c",
            ts=datetime(2026, 1, 1), duration_s=8.0,
        )
        parent = ExecutionNode(
            node_id="p", node_type="llm_response", event_id="p",
            ts=datetime(2026, 1, 1), duration_s=10.0,
            children=[child],
        )
        uq = ExecutionNode(
            node_id="uq", node_type="user_query", event_id="uq",
            ts=datetime(2026, 1, 1), duration_s=10.0,
            children=[parent],
        )
        root = ExecutionNode(
            node_id="r", node_type="session", event_id=None,
            ts=datetime(2026, 1, 1), duration_s=10.0,
            children=[uq],
        )

        summary = analyzer._build_summary(root)
        # tool_execution gets 8s (leaf), llm_inference gets 2s (self time)
        assert summary.time_by_category.get("tool_execution", 0) == pytest.approx(8.0)
        assert summary.time_by_category.get("llm_inference", 0) == pytest.approx(2.0)

    def test_token_aggregation(self):
        from core.agent.session_analyzer import ExecutionNode
        analyzer = self._make_analyzer()

        llm1 = ExecutionNode(
            node_id="l1", node_type="llm_response", event_id="l1",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            tokens_in=1000, tokens_out=200,
        )
        llm2 = ExecutionNode(
            node_id="l2", node_type="llm_response", event_id="l2",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            tokens_in=500, tokens_out=100,
        )
        root = ExecutionNode(
            node_id="r", node_type="session", event_id=None,
            ts=datetime(2026, 1, 1), duration_s=5.0,
            children=[llm1, llm2],
        )

        summary = analyzer._build_summary(root)
        assert summary.total_tokens == 1800
        assert summary.tokens_by_source["prompt"] == 1500
        assert summary.tokens_by_source["completion"] == 300

    def test_cost_by_turn(self):
        """Each llm_response gets a distinct turn number (depth-first order)."""
        from core.agent.session_analyzer import ExecutionNode
        analyzer = self._make_analyzer()

        llm1 = ExecutionNode(
            node_id="l1", node_type="llm_response", event_id="l1",
            ts=datetime(2026, 1, 1), duration_s=2.0,
            cost_usd=0.005,
        )
        llm2 = ExecutionNode(
            node_id="l2", node_type="llm_response", event_id="l2",
            ts=datetime(2026, 1, 1), duration_s=1.0,
            cost_usd=0.010,
        )
        uq = ExecutionNode(
            node_id="uq", node_type="user_query", event_id="uq",
            ts=datetime(2026, 1, 1), duration_s=5.0,
            children=[llm1, llm2],
        )
        root = ExecutionNode(
            node_id="r", node_type="session", event_id=None,
            ts=datetime(2026, 1, 1), duration_s=5.0,
            children=[uq],
        )

        summary = analyzer._build_summary(root)
        assert summary.total_cost_usd == pytest.approx(0.015)
        assert summary.cost_by_turn[1] == pytest.approx(0.005)
        assert summary.cost_by_turn[2] == pytest.approx(0.010)

    def test_root_causes_from_slow_nodes(self):
        from core.agent.session_analyzer import ExecutionNode, SLOW_NODE_THRESHOLD_S
        analyzer = self._make_analyzer()

        slow = ExecutionNode(
            node_id="s", node_type="tool_result", event_id="s",
            ts=datetime(2026, 1, 1), duration_s=SLOW_NODE_THRESHOLD_S + 5,
            detail="slow_tool",
            issues=["SLOW"],
        )
        root = ExecutionNode(
            node_id="r", node_type="session", event_id=None,
            ts=datetime(2026, 1, 1), duration_s=20.0,
            children=[slow],
        )

        summary = analyzer._build_summary(root)
        assert any("slow_tool" in c for c in summary.root_causes)

    def test_empty_tree_summary(self):
        from core.agent.session_analyzer import ExecutionNode
        analyzer = self._make_analyzer()

        root = ExecutionNode(
            node_id="r", node_type="session", event_id=None,
            ts=datetime(2026, 1, 1), duration_s=0,
        )
        summary = analyzer._build_summary(root)
        assert summary.total_tokens == 0
        assert summary.total_cost_usd == 0.0
        assert summary.total_duration_s == 0
        assert summary.root_causes == []


class TestRenderSummary:
    """Unit tests for SessionReport._render_summary."""

    def test_renders_time_breakdown(self):
        from core.agent.session_analyzer import SessionReport, ExecutionSummary
        summary = ExecutionSummary(
            total_duration_s=10.0,
            time_by_category={"llm_inference": 7.0, "tool_execution": 3.0},
            bottleneck_category="llm_inference",
            total_tokens=0,
            tokens_by_source={},
            largest_token_source=None,
            total_cost_usd=0,
            cost_by_turn={},
            root_causes=[],
        )
        report = SessionReport(
            session_id="test", timeline=[], total_duration_s=10.0,
            issues=[], recommendations=[], stats={},
        )
        lines = report._render_summary(summary)
        text = "\n".join(lines)
        assert "10.0s" in text
        assert "BOTTLENECK" in text
        assert "llm_inference" in text

    def test_renders_token_breakdown(self):
        from core.agent.session_analyzer import SessionReport, ExecutionSummary
        summary = ExecutionSummary(
            total_duration_s=5.0,
            time_by_category={},
            bottleneck_category=None,
            total_tokens=1500,
            tokens_by_source={"prompt": 1200, "completion": 300},
            largest_token_source="prompt",
            total_cost_usd=0,
            cost_by_turn={},
            root_causes=[],
        )
        report = SessionReport(
            session_id="test", timeline=[], total_duration_s=5.0,
            issues=[], recommendations=[], stats={},
        )
        lines = report._render_summary(summary)
        text = "\n".join(lines)
        assert "1,500" in text
        assert "LARGEST CONTRIBUTOR" in text


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
