"""Tests for _persist_turn_events, _recover_history_from_db, and _SessionCache."""

import json
from unittest.mock import MagicMock, patch, call

import pytest


# ---------------------------------------------------------------------------
# _recover_history_from_db
# ---------------------------------------------------------------------------


class TestRecoverHistory:
    """Verify _recover_history_from_db reconstructs valid OpenAI message sequences."""

    def _make_row(self, event_type, content, metadata=None):
        return (event_type, content, json.dumps(metadata or {}))

    def _mock_db(self, rows):
        """Create a mock db where ORM query returns rows on the fallback path.

        Snapshot query (.first()) returns None → triggers fallback.
        Fallback query (.all()) returns the provided rows.
        """
        db = MagicMock()
        chain = db.query.return_value.filter.return_value.order_by.return_value
        chain.first.return_value = None  # no snapshot
        chain.limit.return_value.all.return_value = rows
        return db

    def test_text_only_conversation(self):
        """Pure text conversation recovers correctly."""
        rows = [
            self._make_row("user_query", "Hello"),
            self._make_row("llm_response", "Hi there!"),
            self._make_row("user_query", "How are you?"),
            self._make_row("llm_response", "I'm good."),
        ]
        db = self._mock_db(rows)
        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.context.prompt_assembler.PromptAssembler") as mock_pa,
        ):
            mock_pa.return_value.assemble.return_value = MagicMock(
                system_message="system",
                snapshot_id=None,
                token_breakdown={},
                sections={"identity": "system"},
            )
            from api.routers.chat import _recover_history_from_db

            history, sections = _recover_history_from_db(db, "u1", "s1")

        assert sections is not None
        assert history[0] == {"role": "system", "content": "system"}
        assert history[1] == {"role": "user", "content": "Hello"}
        assert history[2] == {"role": "assistant", "content": "Hi there!"}
        assert history[3] == {"role": "user", "content": "How are you?"}
        assert history[4] == {"role": "assistant", "content": "I'm good."}

    def test_tool_call_roundtrip(self):
        """user → tool_call → tool_result → llm_response recovers correctly."""
        rows = [
            self._make_row("user_query", "Read file.txt"),
            self._make_row(
                "tool_call",
                json.dumps(
                    {"tool_call_id": "tc1", "name": "read_file", "arguments": '{"path":"f.txt"}'}
                ),
                {"tool_call_id": "tc1", "name": "read_file"},
            ),
            self._make_row(
                "tool_result",
                json.dumps({"result": "file contents"}),
                {"tool_call_id": "tc1", "name": "read_file"},
            ),
            self._make_row("llm_response", "The file says: file contents"),
        ]
        db = self._mock_db(rows)
        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.context.prompt_assembler.PromptAssembler") as mock_pa,
        ):
            mock_pa.return_value.assemble.return_value = MagicMock(
                system_message="sys",
                snapshot_id=None,
                token_breakdown={},
                sections={"identity": "sys"},
            )
            from api.routers.chat import _recover_history_from_db

            history, sections = _recover_history_from_db(db, "u1", "s1")

        # system, user, assistant(tool_calls), tool, assistant
        assert len(history) == 5
        assert history[2]["role"] == "assistant"
        assert len(history[2]["tool_calls"]) == 1
        assert history[2]["tool_calls"][0]["id"] == "tc1"
        assert history[3]["role"] == "tool"
        assert history[3]["tool_call_id"] == "tc1"
        assert history[4] == {"role": "assistant", "content": "The file says: file contents"}

    def test_empty_session_returns_empty(self):
        db = self._mock_db([])
        with patch("api.routers.chat.SessionLocal"):
            from api.routers.chat import _recover_history_from_db

            assert _recover_history_from_db(db, "u1", "s1") == ([], None)


# ---------------------------------------------------------------------------
# _append_recovered_events — trailing tool_calls
# ---------------------------------------------------------------------------


class TestAppendRecoveredEvents:
    """Verify _append_recovered_events handles edge cases in DB event reconstruction."""

    def _make_row(self, event_type, content, metadata=None):
        return (event_type, content, json.dumps(metadata or {}))

    def test_trailing_tool_calls_flushed(self):
        """DB ends with tool_call events (no tool_result) — API crashed mid-execution.
        The pending tool_calls must be flushed as an assistant message so
        _merge_tool_results_into_history can heal or merge them later."""
        from core.history_utils import append_recovered_events as _append_recovered_events

        history = [{"role": "system", "content": "sys"}]
        rows = [
            self._make_row("user_query", "do something"),
            self._make_row(
                "tool_call",
                json.dumps({"tool_call_id": "tc_trail", "name": "bash", "arguments": "{}"}),
                {"tool_call_id": "tc_trail", "name": "bash"},
            ),
        ]
        result = _append_recovered_events(history, rows)
        # Should have: system, user, assistant(tool_calls)
        assert len(result) == 3
        assert result[2]["role"] == "assistant"
        assert len(result[2]["tool_calls"]) == 1
        assert result[2]["tool_calls"][0]["id"] == "tc_trail"

    def test_trailing_multiple_tool_calls_flushed(self):
        """Multiple trailing tool_calls flushed as one assistant message."""
        from core.history_utils import append_recovered_events as _append_recovered_events

        history = [{"role": "system", "content": "sys"}]
        rows = [
            self._make_row("user_query", "read files"),
            self._make_row(
                "tool_call",
                json.dumps({"tool_call_id": "tc_a", "name": "read_file"}),
                {"tool_call_id": "tc_a"},
            ),
            self._make_row(
                "tool_call",
                json.dumps({"tool_call_id": "tc_b", "name": "read_file"}),
                {"tool_call_id": "tc_b"},
            ),
        ]
        result = _append_recovered_events(history, rows)
        assert result[2]["role"] == "assistant"
        tc_ids = {tc["id"] for tc in result[2]["tool_calls"]}
        assert tc_ids == {"tc_a", "tc_b"}


# ---------------------------------------------------------------------------
# _persist_turn_events
# ---------------------------------------------------------------------------


class TestPersistTurnEvents:
    """Verify _persist_turn_events writes tool_call events and clean llm_response."""

    def test_writes_tool_call_events(self):
        """Each tool_call in the response should produce a tool_call event."""
        tool_calls = [
            {"id": "tc1", "function": {"name": "read_file", "arguments": '{"path":"a.txt"}'}},
            {"id": "tc2", "function": {"name": "list_dir", "arguments": '{"path":"."}'}},
        ]
        created_events = []

        with (
            patch("api.routers.chat.SessionLocal") as mock_sl,
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_stream_event.side_effect = lambda **kw: (
                created_events.append(kw) or MagicMock()
            )
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "Read files"}],
                None,
                "Here are the files",
                tool_calls,
            )

        # Should have tool_call events for each tool call
        tc_events = [e for e in created_events if e.get("event_type") == "tool_call"]
        assert len(tc_events) == 2
        tc1_content = json.loads(tc_events[0]["content"])
        assert tc1_content["tool_call_id"] == "tc1"
        assert tc1_content["name"] == "read_file"

    def test_llm_response_has_no_tool_calls_suffix(self):
        """llm_response content should NOT have [tool_calls: ...] appended."""
        tool_calls = [{"id": "tc1", "function": {"name": "read_file", "arguments": "{}"}}]
        llm_content = None

        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks"),
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_stream_event.return_value = MagicMock()

            def capture_llm(**kw):
                nonlocal llm_content
                llm_content = kw.get("content")
                return MagicMock()

            mock_el.create_llm_response.side_effect = capture_llm
            mock_el_cls.return_value = mock_el

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "hi"}],
                None,
                "response text",
                tool_calls,
            )

        assert llm_content == "response text"
        assert "[tool_calls:" not in (llm_content or "")


# ---------------------------------------------------------------------------
# _SessionCache
# ---------------------------------------------------------------------------


class TestSessionCache:
    def test_history_and_tools_evict_together(self):
        from api.routers.chat import _SessionCache

        cache = _SessionCache(maxsize=2, ttl=9999)
        cache["s1"] = {"history": [{"role": "system"}], "tools": [{"t": 1}]}
        cache["s2"] = {"history": [{"role": "system"}], "tools": [{"t": 2}]}
        cache["s3"] = {"history": [{"role": "system"}], "tools": [{"t": 3}]}
        # s1 should be evicted
        assert cache.get("s1") is None
        s2 = cache.get("s2")
        assert s2 is not None
        assert s2["tools"] == [{"t": 2}]

    def test_ttl_expiry(self):
        from api.routers.chat import _SessionCache

        cache = _SessionCache(maxsize=100, ttl=1)
        with patch("time.monotonic", return_value=100.0):
            cache["s1"] = {"history": ["h"], "tools": ["t"]}
        with patch("time.monotonic", return_value=100.5):
            assert cache.get("s1") is not None
        # 101.6 is >1s after the refreshed ts (100.5)
        with patch("time.monotonic", return_value=101.6):
            assert cache.get("s1") is None

    def test_access_refreshes_ttl(self):
        from api.routers.chat import _SessionCache

        cache = _SessionCache(maxsize=100, ttl=2)
        with patch("time.monotonic", return_value=100.0):
            cache["s1"] = {"history": ["h"], "tools": ["t"]}
        # Access at t=101 refreshes ts
        with patch("time.monotonic", return_value=101.0):
            assert cache.get("s1") is not None
        # t=102: 1s since last access, still alive
        with patch("time.monotonic", return_value=102.0):
            assert cache.get("s1") is not None
        # t=104.1: >2s since last access at t=102, expired
        with patch("time.monotonic", return_value=104.1):
            assert cache.get("s1") is None


# ---------------------------------------------------------------------------
# Observer receives correct messages in edge-cloud /chat/turn path
# ---------------------------------------------------------------------------


class TestObserverReceivesMessages:
    """Observer runs only on final reply (no tool_calls + has full_text)."""

    def test_observer_gets_user_and_assistant_on_final_reply(self):
        """Final reply (no tool_calls): observer gets user query + assistant text."""
        observer_messages = []

        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks") as mock_hooks_cls,
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_stream_event.return_value = MagicMock()
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            mock_hooks = MagicMock()
            mock_hooks.run_observer.side_effect = lambda sid, uid, msgs, **kw: (
                observer_messages.extend(msgs)
            )
            mock_hooks_cls.return_value = mock_hooks

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "What is event sourcing?"}],
                None,
                "Event sourcing is a pattern...",
                [],  # no tool_calls = final reply
            )

        assert len(observer_messages) == 2
        assert observer_messages[0] == {"role": "user", "content": "What is event sourcing?"}
        assert observer_messages[1] == {
            "role": "assistant",
            "content": "Event sourcing is a pattern...",
        }

    def test_observer_skipped_on_intermediate_tool_turn(self):
        """Intermediate turn (has tool_calls): observer should NOT run,
        even if there's full_text."""
        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks") as mock_hooks_cls,
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_stream_event.return_value = MagicMock()
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            mock_hooks = MagicMock()
            mock_hooks_cls.return_value = mock_hooks

            from api.routers.chat import _persist_turn_events

            # LLM returned text AND tool_calls — intermediate turn
            _persist_turn_events(
                "u1",
                "s1",
                [{"role": "user", "content": "fix the bug"}],
                None,
                "Let me read the file first.",
                [{"id": "tc1", "function": {"name": "read_file", "arguments": "{}"}}],
            )

        mock_hooks.run_observer.assert_not_called()

    def test_observer_runs_on_final_reply_without_user_content(self):
        """Final reply on a tool-result-only turn (no user message but has
        full_text and no tool_calls): observer should run with assistant text."""
        observer_messages = []

        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks") as mock_hooks_cls,
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_stream_event.return_value = MagicMock()
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            mock_hooks = MagicMock()
            mock_hooks.run_observer.side_effect = lambda sid, uid, msgs, **kw: (
                observer_messages.extend(msgs)
            )
            mock_hooks_cls.return_value = mock_hooks

            from api.routers.chat import _persist_turn_events

            # Tool-result turn, LLM gives final answer (no more tool_calls)
            _persist_turn_events(
                "u1",
                "s1",
                [],
                [{"tool_call_id": "tc1", "name": "read_file", "result": "content"}],
                "The file contains configuration settings.",
                [],  # no tool_calls = final
            )

        assert len(observer_messages) == 1
        assert observer_messages[0]["role"] == "assistant"
        assert "configuration" in observer_messages[0]["content"]

    def test_observer_skipped_when_no_text(self):
        """No full_text at all: observer should NOT run."""
        with (
            patch("api.routers.chat.SessionLocal"),
            patch("core.events.event_logger.EventLogger") as mock_el_cls,
            patch("core.agent.turn_hooks.TurnHooks") as mock_hooks_cls,
        ):
            mock_el = MagicMock()
            mock_el.create_user_query.return_value = MagicMock(
                event_id="ev1", causal_chain_id="cc1"
            )
            mock_el.create_stream_event.return_value = MagicMock()
            mock_el.create_llm_response.return_value = MagicMock()
            mock_el_cls.return_value = mock_el

            mock_hooks = MagicMock()
            mock_hooks_cls.return_value = mock_hooks

            from api.routers.chat import _persist_turn_events

            _persist_turn_events(
                "u1",
                "s1",
                [],
                [{"tool_call_id": "tc1", "name": "read_file", "result": "data"}],
                "",
                [{"id": "tc2", "function": {"name": "write_file", "arguments": "{}"}}],
            )

        mock_hooks.run_observer.assert_not_called()
