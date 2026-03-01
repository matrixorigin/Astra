"""Tests for chat turn integration with RichRenderer."""

from io import StringIO
from dataclasses import dataclass, field
from typing import Any

import pytest
from rich.console import Console

from cli.edge_chat_loop import edge_chat_loop, _consume_turn
from cli.permissions import PermissionManager
from cli.tools.router import ToolRouter
from cli.tools.file_ops import register_file_tools
from cli.tools.shell import register_shell_tools
from cli.ui.renderer import RichRenderer


class MockAPIClient:
    """Mock API client returning scripted SSE responses."""

    def __init__(self, turns: list[list[dict[str, Any]]]):
        self._turns = turns
        self._call_count = 0

    async def chat_turn(self, **kwargs):
        events = self._turns[self._call_count] if self._call_count < len(self._turns) else []
        self._call_count += 1
        for e in events:
            yield e


def _make_renderer() -> tuple[RichRenderer, Console, StringIO]:
    buf = StringIO()
    console = Console(file=buf, force_terminal=True, width=80)
    return RichRenderer(console=console), console, buf


@pytest.fixture
def project(tmp_path):
    (tmp_path / "hello.txt").write_text("Hello, world!\n")
    return tmp_path


@pytest.fixture
def router(project):
    r = ToolRouter()
    register_file_tools(r, str(project))
    register_shell_tools(r, str(project))
    return r


@pytest.fixture
def perms():
    return PermissionManager(auto_approve=True)


class TestRichRendererWithEdgeLoop:
    @pytest.mark.asyncio
    async def test_text_only_turn(self, router, perms):
        """Single turn text response renders through RichRenderer."""
        renderer, console, buf = _make_renderer()
        api = MockAPIClient([[
            {"type": "text_delta", "content": "The answer is 42."},
            {"type": "turn_complete", "has_tool_calls": False},
        ]])
        renderer.begin_response()
        result = await edge_chat_loop(
            "what is 42?", api, router, perms,
            session_id="s1", renderer=renderer,
        )
        renderer.end_response()
        assert "42" in result.text

    @pytest.mark.asyncio
    async def test_tool_call_turn(self, router, perms, project):
        """Turn with tool call renders tool start/done."""
        renderer, console, buf = _make_renderer()
        api = MockAPIClient([
            # Turn 1: tool call
            [
                {"type": "text_delta", "content": "Let me read that."},
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            # Turn 2: final answer
            [
                {"type": "text_delta", "content": "The file says Hello."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "read hello.txt", api, router, perms,
            session_id="s1", project_root=str(project), renderer=renderer,
        )
        output = buf.getvalue()
        assert "read_file" in output
        assert "Hello" in result.text

    @pytest.mark.asyncio
    async def test_consume_turn_with_rich(self):
        """_consume_turn works with RichRenderer."""
        renderer, console, buf = _make_renderer()

        async def stream():
            yield {"type": "text_delta", "content": "Hello "}
            yield {"type": "text_delta", "content": "world"}
            yield {"type": "turn_complete", "has_tool_calls": False}

        renderer.begin_response()
        result = await _consume_turn(stream(), renderer)
        renderer.end_response()
        assert result.text == "Hello world"
        assert not result.has_tool_calls

    @pytest.mark.asyncio
    async def test_multi_turn(self, router, perms, project):
        """Multi-turn with tool calls accumulates correctly."""
        renderer, console, buf = _make_renderer()
        api = MockAPIClient([
            [
                {"type": "tool_call", "id": "tc_1", "name": "read_file",
                 "arguments": {"path": "hello.txt"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "tool_call", "id": "tc_2", "name": "bash",
                 "arguments": {"command": "echo done"}},
                {"type": "turn_complete", "has_tool_calls": True},
            ],
            [
                {"type": "text_delta", "content": "All done."},
                {"type": "turn_complete", "has_tool_calls": False},
            ],
        ])
        result = await edge_chat_loop(
            "do stuff", api, router, perms,
            session_id="s1", project_root=str(project), renderer=renderer,
        )
        assert "All done" in result.text
        output = buf.getvalue()
        assert "read_file" in output
        assert "bash" in output
