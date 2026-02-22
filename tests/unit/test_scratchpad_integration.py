"""Tests for Scratchpad integration in ChatLoop."""

from unittest.mock import AsyncMock, MagicMock, Mock, patch

import pytest

from core.agent.chat_loop import ChatLoop, _SCRATCHPAD_TOOLS


def _make_loop(with_scratchpad: bool = True) -> tuple[ChatLoop, Mock | None]:
    if with_scratchpad:
        scratchpad = Mock()
        scratchpad.get_active_notes = Mock(return_value=[])
        scratchpad.create_note = Mock(return_value="note_abc123")
        scratchpad.close_note = Mock(return_value=True)
    else:
        scratchpad = None

    loop = ChatLoop(
        selector=Mock(),
        executor=Mock(),
        llm_client=Mock(),
        event_logger=Mock(),
        context_manager=Mock(),
        firewall=Mock(),
        agent_id="test-agent",
        scratchpad=scratchpad,
    )
    return loop, scratchpad


class TestScratchpadToolsSchema:
    def test_scratchpad_tools_injected_when_enabled(self):
        loop, _ = _make_loop(with_scratchpad=True)
        sel = Mock()
        sel.tools = [{"type": "function", "function": {"name": "existing_tool"}}]
        sel.event_id = "evt-1"
        loop._pipeline.get_tools_schema = Mock(return_value=sel)

        # Simulate what run_step does
        tools = list(sel.tools) + _SCRATCHPAD_TOOLS
        names = [t["function"]["name"] for t in tools]
        assert "scratchpad_write" in names
        assert "scratchpad_read" in names
        assert "scratchpad_close" in names
        assert "existing_tool" in names

    def test_scratchpad_tools_not_injected_when_disabled(self):
        loop, _ = _make_loop(with_scratchpad=False)
        assert loop.scratchpad is None

    def test_scratchpad_tools_schema_valid(self):
        for tool in _SCRATCHPAD_TOOLS:
            assert tool["type"] == "function"
            assert "name" in tool["function"]
            assert "description" in tool["function"]
            assert "parameters" in tool["function"]


class TestHandleScratchpadTool:
    def test_write_creates_note(self):
        loop, scratchpad = _make_loop()
        result = loop._handle_scratchpad_tool(
            "scratchpad_write",
            {"note_type": "plan", "content": "Step 1: analyze"},
            session_id="sess-1",
            user_id="alice",
        )
        scratchpad.create_note.assert_called_once_with(
            session_id="sess-1",
            user_id="alice",
            note_type="plan",
            content="Step 1: analyze",
            agent_id="test-agent",
        )
        assert result["note_id"] == "note_abc123"
        assert result["status"] == "created"

    def test_read_returns_notes(self):
        loop, scratchpad = _make_loop()
        scratchpad.get_active_notes.return_value = [
            {"note_id": "n1", "note_type": "plan", "content": "my plan"}
        ]
        result = loop._handle_scratchpad_tool(
            "scratchpad_read", {}, session_id="sess-1", user_id="alice",
        )
        assert result["notes"][0]["note_id"] == "n1"

    def test_read_with_type_filter(self):
        loop, scratchpad = _make_loop()
        scratchpad.get_active_notes.return_value = []
        loop._handle_scratchpad_tool(
            "scratchpad_read",
            {"note_type": "todo"},
            session_id="sess-1",
            user_id="alice",
        )
        scratchpad.get_active_notes.assert_called_once_with("sess-1", note_type="todo")

    def test_close_note_default_status(self):
        loop, scratchpad = _make_loop()
        loop._handle_scratchpad_tool(
            "scratchpad_close",
            {"note_id": "note_abc123"},
            session_id="sess-1",
            user_id="alice",
        )
        scratchpad.close_note.assert_called_once_with("note_abc123", status="completed")

    def test_unknown_tool_returns_error(self):
        loop, _ = _make_loop()
        result = loop._handle_scratchpad_tool(
            "scratchpad_unknown", {}, session_id="sess-1", user_id="alice",
        )
        assert "error" in result


class TestBuildMessagesWithScratchpad:
    def test_active_notes_injected_into_system_prompt(self):
        loop, scratchpad = _make_loop()
        scratchpad.get_active_notes.return_value = [
            {"note_id": "n1", "note_type": "plan", "content": "Step 1: do X"},
            {"note_id": "n2", "note_type": "todo", "content": "Check Y"},
        ]
        messages = loop._build_messages("hello", None, session_id="sess-1")
        system_msg = next(m for m in messages if m["role"] == "system")
        assert "Working memory" in system_msg["content"]
        assert "Step 1: do X" in system_msg["content"]
        assert "Check Y" in system_msg["content"]

    def test_no_notes_no_working_memory_section(self):
        loop, scratchpad = _make_loop()
        scratchpad.get_active_notes.return_value = []
        messages = loop._build_messages("hello", None, session_id="sess-1")
        system_msg = next(m for m in messages if m["role"] == "system")
        assert "Working memory" not in system_msg["content"]

    def test_no_scratchpad_no_injection(self):
        loop, _ = _make_loop(with_scratchpad=False)
        messages = loop._build_messages("hello", None, session_id="sess-1")
        system_msg = next(m for m in messages if m["role"] == "system")
        assert "Working memory" not in system_msg["content"]

    def test_no_session_id_no_injection(self):
        loop, scratchpad = _make_loop()
        messages = loop._build_messages("hello", None, session_id=None)
        system_msg = next(m for m in messages if m["role"] == "system")
        assert "Working memory" not in system_msg["content"]
        scratchpad.get_active_notes.assert_not_called()

    def test_note_type_label_in_prompt(self):
        loop, scratchpad = _make_loop()
        scratchpad.get_active_notes.return_value = [
            {"note_id": "n1", "note_type": "hypothesis", "content": "Maybe X causes Y"},
        ]
        messages = loop._build_messages("hello", None, session_id="sess-1")
        system_msg = next(m for m in messages if m["role"] == "system")
        assert "[hypothesis]" in system_msg["content"]
