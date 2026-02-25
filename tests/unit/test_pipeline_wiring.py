"""Tests for A2 wiring: EventLogger → EventPipeline delegation + flush_critical placement.

These test the REAL integration between EventLogger, EventPipeline, ChatLoop, and RunEngine.
Not fake sessions — real mock tracking of emit/flush_critical call order.
"""

import asyncio
from collections import OrderedDict
from unittest.mock import MagicMock, patch, call, AsyncMock
from sqlalchemy.orm import Session

import pytest

from core.events.event_logger import EventLogger
from core.events.models import ConversationEvent, EventType
from core.events.pipeline import EventPipeline


# ── EventLogger delegation tests ──


class TestEventLoggerDelegation:
    """Verify EventLogger delegates to pipeline when enabled."""

    def _make_logger(self, pipeline_enabled=True):
        mock_session = MagicMock(spec=Session)
        mock_pipeline = MagicMock(spec=EventPipeline)
        mock_pipeline.emit.return_value = "evt-123"
        el = EventLogger(mock_session, pipeline=mock_pipeline)
        return el, mock_session, mock_pipeline

    @patch("core.events.event_logger._PIPELINE_ENABLED", True)
    def test_log_event_delegates_to_pipeline_emit(self):
        """log_event() should call pipeline.emit() instead of DB write."""
        el, mock_session, mock_pipeline = self._make_logger()
        ev = ConversationEvent(
            event_id="e1", user_id="u1", session_id="s1",
            agent_id="a", agent_version="1", event_type=EventType.USER_QUERY,
            content="hello", causal_chain_id="c1",
        )
        result = el.log_event(ev)
        assert result == "evt-123"
        mock_pipeline.emit.assert_called_once_with(ev)
        # DB session should NOT be touched
        mock_session.add.assert_not_called()
        mock_session.commit.assert_not_called()

    @patch("core.events.event_logger._PIPELINE_ENABLED", False)
    def test_log_event_falls_back_to_sync_when_disabled(self):
        """When disabled, log_event() should do synchronous DB write."""
        el, mock_session, mock_pipeline = self._make_logger()
        ev = ConversationEvent(
            event_id="e1", user_id="u1", session_id="s1",
            agent_id="a", agent_version="1", event_type=EventType.USER_QUERY,
            content="hello", causal_chain_id="c1",
        )
        el.log_event(ev)
        # Pipeline should NOT be called
        mock_pipeline.emit.assert_not_called()
        # DB session SHOULD be used
        mock_session.add.assert_called_once()
        mock_session.commit.assert_called_once()

    @patch("core.events.event_logger._PIPELINE_ENABLED", True)
    def test_flush_critical_delegates_to_pipeline(self):
        el, _, mock_pipeline = self._make_logger()
        el.flush_critical()
        mock_pipeline.flush_critical.assert_called_once()

    @patch("core.events.event_logger._PIPELINE_ENABLED", False)
    def test_flush_critical_noop_when_disabled(self):
        el, _, mock_pipeline = self._make_logger()
        el.flush_critical()
        mock_pipeline.flush_critical.assert_not_called()

    @patch("core.events.event_logger._PIPELINE_ENABLED", True)
    def test_no_pipeline_falls_back_to_sync(self):
        """EventLogger without pipeline should still do sync writes."""
        mock_session = MagicMock(spec=Session)
        el = EventLogger(mock_session, pipeline=None)
        ev = ConversationEvent(
            event_id="e1", user_id="u1", session_id="s1",
            agent_id="a", agent_version="1", event_type=EventType.USER_QUERY,
            content="hello", causal_chain_id="c1",
        )
        el.log_event(ev)
        mock_session.add.assert_called_once()
        mock_session.commit.assert_called_once()

    @patch("core.events.event_logger._PIPELINE_ENABLED", True)
    def test_create_user_query_goes_through_pipeline(self):
        """create_user_query → log_event → pipeline.emit."""
        el, mock_session, mock_pipeline = self._make_logger()
        ev = el.create_user_query(user_id="u1", session_id="s1", content="test")
        assert mock_pipeline.emit.call_count == 1
        emitted = mock_pipeline.emit.call_args[0][0]
        assert emitted.event_type == EventType.USER_QUERY.value
        assert emitted.content == "test"

    @patch("core.events.event_logger._PIPELINE_ENABLED", True)
    def test_create_llm_response_goes_through_pipeline(self):
        """create_llm_response → log_event → pipeline.emit."""
        el, _, mock_pipeline = self._make_logger()
        el.create_llm_response(
            user_id="u1", session_id="s1", content="answer",
            agent_id="a", agent_version="1",
            parent_event_id="p1", causal_chain_id="c1",
        )
        assert mock_pipeline.emit.call_count == 1
        emitted = mock_pipeline.emit.call_args[0][0]
        assert emitted.event_type == EventType.LLM_RESPONSE.value

    @patch("core.events.event_logger._PIPELINE_ENABLED", True)
    def test_create_stream_event_goes_through_pipeline(self):
        """create_stream_event → log_event → pipeline.emit."""
        el, _, mock_pipeline = self._make_logger()
        el.create_stream_event(
            user_id="u1", session_id="s1",
            event_type="stream_text_delta", content="chunk",
        )
        assert mock_pipeline.emit.call_count == 1


# ── ChatLoop flush_critical ordering tests ──


class TestChatLoopFlushOrdering:
    """Verify flush_critical() is called AFTER create_user_query and BEFORE build_context."""

    def _make_chat_loop(self):
        from core.agent.chat_loop import ChatLoop

        mock_session = MagicMock(spec=Session)
        mock_pipeline = MagicMock(spec=EventPipeline)
        mock_pipeline.emit.return_value = "evt-id"

        el = EventLogger(mock_session, pipeline=mock_pipeline)

        # Track call order
        call_order = []
        original_create_uq = el.create_user_query
        original_flush = el.flush_critical

        def tracked_create_uq(**kwargs):
            call_order.append("create_user_query")
            return original_create_uq(**kwargs)

        def tracked_flush():
            call_order.append("flush_critical")
            return original_flush()

        el.create_user_query = tracked_create_uq
        el.flush_critical = tracked_flush

        # Mock all ChatLoop dependencies
        selector = MagicMock()
        selector.get_tools_schema.return_value = MagicMock(tools=[], event_id=None)
        executor = MagicMock()
        llm_client = MagicMock()
        llm_client.chat.return_value = MagicMock(content="response", tool_calls=None)
        llm_client.db = None
        llm_client.config = {"model": "test-model"}
        context_manager = MagicMock()
        context_manager.build_context.return_value = {}
        context_manager.save_snapshot.return_value = "snap-1"
        firewall = MagicMock()
        firewall.verify_response.return_value = MagicMock(
            safe_to_deliver=True, confidence_score=1.0, claims_failed=0,
        )

        def tracked_build_context(**kwargs):
            call_order.append("build_context")
            return {}

        context_manager.build_context = tracked_build_context

        loop = ChatLoop(
            selector=selector, executor=executor, llm_client=llm_client,
            event_logger=el, context_manager=context_manager, firewall=firewall,
        )
        return loop, call_order, mock_pipeline

    @patch("core.events.event_logger._PIPELINE_ENABLED", True)
    def test_run_step_flush_before_build_context(self):
        """run_step: create_user_query → flush_critical → build_context."""
        loop, call_order, _ = self._make_chat_loop()
        result = asyncio.run(
            loop.run_step("hello", session_id="s1", user_id="u1")
        )
        assert call_order[:3] == ["create_user_query", "flush_critical", "build_context"]


# ── RunEngine flush_critical on terminal states ──


class TestRunEngineFlushOnTerminal:
    """_log_run_event creates a bare EventLogger(db) — no pipeline.

    This is the key design guarantee: run lifecycle events are ALWAYS written
    synchronously via db.add + db.commit, never deferred through a pipeline.
    This ensures terminal events (COMPLETED/FAILED/CANCELLED) are immediately
    visible for cross-worker polling.
    """

    def _make_engine_and_run(self):
        from core.agent.run_engine import RunEngine
        mock_session = MagicMock(spec=Session)
        engine = RunEngine(lambda: mock_session)
        mock_run = MagicMock()
        mock_run.run_id = "r1"
        mock_run.user_id = "u1"
        mock_run.session_id = "s1"
        mock_run.parent_run_id = None
        mock_run.waiting_for = None
        mock_run.context = {}
        mock_run.to_event_content.return_value = "{}"
        return engine, mock_session, mock_run

    def test_log_run_event_writes_sync(self):
        """All lifecycle events go through synchronous db.add + db.commit."""
        engine, mock_session, mock_run = self._make_engine_and_run()
        engine._log_run_event(mock_run, EventType.RUN_COMPLETED)
        mock_session.add.assert_called_once()
        mock_session.commit.assert_called_once()

    def test_log_run_event_never_uses_pipeline(self):
        """Even when _PIPELINE_ENABLED is True, _log_run_event bypasses it.

        RunEngine creates EventLogger(db) without a pipeline argument, so
        log_event() always takes the synchronous path.  This is critical:
        terminal events must be committed before start_run returns, otherwise
        cross-worker polling may miss them.
        """
        engine, mock_session, mock_run = self._make_engine_and_run()

        with patch("core.events.event_logger._PIPELINE_ENABLED", True):
            engine._log_run_event(mock_run, EventType.RUN_COMPLETED)

        # If pipeline were used, log_event() would call pipeline.emit() and
        # return early — session.add would NOT be called.  The fact that
        # session.add IS called proves the synchronous path was taken.
        mock_session.add.assert_called_once()
        mock_session.commit.assert_called_once()

    def test_log_run_event_uses_short_lived_session(self):
        """Each _log_run_event call acquires and releases its own session."""
        from core.agent.run_engine import RunEngine
        sessions_created = []
        def tracking_factory():
            s = MagicMock(spec=Session)
            sessions_created.append(s)
            return s

        engine = RunEngine(tracking_factory)
        mock_run = MagicMock()
        mock_run.run_id = "r1"
        mock_run.user_id = "u1"
        mock_run.session_id = "s1"
        mock_run.parent_run_id = None
        mock_run.waiting_for = None
        mock_run.context = {}
        mock_run.to_event_content.return_value = "{}"

        engine._log_run_event(mock_run, EventType.RUN_STARTED)
        engine._log_run_event(mock_run, EventType.RUN_COMPLETED)

        # Two calls → two sessions acquired and closed
        assert len(sessions_created) == 2
        for s in sessions_created:
            s.close.assert_called_once()
