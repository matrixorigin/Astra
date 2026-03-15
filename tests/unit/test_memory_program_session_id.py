"""Regression tests for Bug 6: memory_program inject must propagate session_id.

Without session_id, user-requested memories (e.g. "remember I prefer Python")
get session_id=NULL and become cross-session global memories instead of
being scoped to the current conversation.
"""

from __future__ import annotations

import json
import pytest
from unittest.mock import Mock, patch


@pytest.mark.asyncio
class TestMemoryProgramSessionId:
    """Bug 6: session_id must flow from tool router → memory_program → inject."""

    async def test_inject_receives_session_id_from_router(self):
        """When router passes session_id, inject must receive it."""
        from cli.tools.router import ToolRouter, ToolCall
        from cli.tools.memory_program import MemoryProgramTool

        router = ToolRouter()
        router.register(MemoryProgramTool())

        with patch("core.memory.factory.create_editor") as mock_create:
            mock_editor = Mock()
            mock_editor.inject.return_value = {"memory_id": "m1"}
            mock_create.return_value = mock_editor

            call = ToolCall(
                id="c1",
                name="memory_program",
                arguments={"actions": [{"operation": "inject", "content": "I prefer Python"}]},
            )
            await router.execute([call], user_id="user1", session_id="sess-xyz")

        mock_editor.inject.assert_called_once()
        call_kwargs = mock_editor.inject.call_args.kwargs
        assert call_kwargs.get("session_id") == "sess-xyz", (
            "session_id must be passed to inject so memory is session-scoped"
        )

    async def test_inject_session_id_none_when_not_provided(self):
        """Without session_id in router call, inject gets session_id=None."""
        from cli.tools.router import ToolRouter, ToolCall
        from cli.tools.memory_program import MemoryProgramTool

        router = ToolRouter()
        router.register(MemoryProgramTool())

        with patch("core.memory.factory.create_editor") as mock_create:
            mock_editor = Mock()
            mock_editor.inject.return_value = {"memory_id": "m1"}
            mock_create.return_value = mock_editor

            call = ToolCall(
                id="c1",
                name="memory_program",
                arguments={"actions": [{"operation": "inject", "content": "test"}]},
            )
            await router.execute([call], user_id="user1")  # no session_id

        call_kwargs = mock_editor.inject.call_args.kwargs
        assert call_kwargs.get("session_id") is None

    async def test_router_passes_session_id_to_kwargs(self):
        """ToolRouter._execute_one must add session_id to kwargs for EdgeTools."""
        from cli.tools.router import ToolRouter, ToolCall
        from cli.tools.base import EdgeTool, SideEffect

        class CaptureTool(EdgeTool):
            name = "capture"
            description = "test"
            parameters = {"type": "object", "properties": {}}
            side_effect = SideEffect.READ
            captured = {}

            async def execute(self, **kwargs):
                CaptureTool.captured = kwargs
                return "ok"

        router = ToolRouter()
        router.register(CaptureTool())

        call = ToolCall(id="c1", name="capture", arguments={})
        await router.execute([call], user_id="u1", session_id="s1")

        assert CaptureTool.captured.get("session_id") == "s1"
        assert CaptureTool.captured.get("user_id") == "u1"
