"""Unit tests for ToolRouter user_id propagation."""

import pytest
from unittest.mock import Mock

from cli.tools.router import ToolRouter, ToolCall
from cli.tools.base import EdgeTool, SideEffect


class TestToolRouterUserIdPropagation:
    """Test that ToolRouter correctly propagates user_id to EdgeTools."""

    @pytest.mark.asyncio
    async def test_execute_passes_user_id_to_edge_tool(self):
        """Test that ToolRouter.execute passes user_id to EdgeTool.execute."""

        class MockTool(EdgeTool):
            name = "mock_tool"
            description = "Mock tool for testing"
            parameters = {"type": "object", "properties": {}}
            side_effect = SideEffect.READ

            def __init__(self):
                self.execute_called_with = None

            async def execute(self, **kwargs):
                self.execute_called_with = kwargs
                return "success"

        router = ToolRouter()
        mock_tool = MockTool()
        router.register(mock_tool)

        call = ToolCall(id="call-1", name="mock_tool", arguments={})
        test_user_id = "user-123-abc"

        results = await router.execute([call], user_id=test_user_id)
        result = results[0]

        # Verify user_id was passed to tool
        assert mock_tool.execute_called_with is not None
        assert mock_tool.execute_called_with["user_id"] == test_user_id
        assert not result.error

    @pytest.mark.asyncio
    async def test_execute_without_user_id_passes_none(self):
        """Test that when no user_id provided, None is passed to tool."""

        class MockTool(EdgeTool):
            name = "mock_tool"
            description = "Mock tool for testing"
            parameters = {"type": "object", "properties": {}}
            side_effect = SideEffect.READ

            def __init__(self):
                self.execute_called_with = None

            async def execute(self, **kwargs):
                self.execute_called_with = kwargs
                return "success"

        router = ToolRouter()
        mock_tool = MockTool()
        router.register(mock_tool)

        call = ToolCall(id="call-2", name="mock_tool", arguments={})

        results = await router.execute([call])  # No user_id
        result = results[0]

        # Verify user_id is not in kwargs when not provided
        assert mock_tool.execute_called_with is not None
        assert "user_id" not in mock_tool.execute_called_with
        assert not result.error

    @pytest.mark.asyncio
    async def test_execute_preserves_other_kwargs(self):
        """Test that user_id doesn't interfere with other kwargs."""

        class MockTool(EdgeTool):
            name = "mock_tool"
            description = "Mock tool for testing"
            parameters = {
                "type": "object",
                "properties": {"param1": {"type": "string"}, "param2": {"type": "integer"}},
            }
            side_effect = SideEffect.READ

            def __init__(self):
                self.execute_called_with = None

            async def execute(self, **kwargs):
                self.execute_called_with = kwargs
                return "success"

        router = ToolRouter()
        mock_tool = MockTool()
        router.register(mock_tool)

        call = ToolCall(id="call-3", name="mock_tool", arguments={"param1": "value1", "param2": 42})
        test_user_id = "user-456-def"

        results = await router.execute([call], user_id=test_user_id)
        result = results[0]

        # Verify all parameters are passed correctly
        expected_kwargs = {"param1": "value1", "param2": 42, "user_id": test_user_id}
        assert mock_tool.execute_called_with == expected_kwargs
        assert not result.error

    @pytest.mark.asyncio
    async def test_multiple_tools_each_get_user_id(self):
        """Test that multiple tools each receive the same user_id."""

        class MockTool1(EdgeTool):
            name = "tool1"
            description = "First mock tool"
            parameters = {"type": "object", "properties": {}}
            side_effect = SideEffect.READ

            def __init__(self):
                self.user_id_received = None

            async def execute(self, **kwargs):
                self.user_id_received = kwargs.get("user_id")
                return "tool1 result"

        class MockTool2(EdgeTool):
            name = "tool2"
            description = "Second mock tool"
            parameters = {"type": "object", "properties": {}}
            side_effect = SideEffect.READ

            def __init__(self):
                self.user_id_received = None

            async def execute(self, **kwargs):
                self.user_id_received = kwargs.get("user_id")
                return "tool2 result"

        router = ToolRouter()
        tool1 = MockTool1()
        tool2 = MockTool2()
        router.register(tool1)
        router.register(tool2)

        test_user_id = "user-789-ghi"

        # Execute both tools
        call1 = ToolCall(id="call-4", name="tool1", arguments={})
        call2 = ToolCall(id="call-5", name="tool2", arguments={})

        results1 = await router.execute([call1], user_id=test_user_id)
        results2 = await router.execute([call2], user_id=test_user_id)

        result1 = results1[0]
        result2 = results2[0]

        # Both tools should receive the same user_id
        assert tool1.user_id_received == test_user_id
        assert tool2.user_id_received == test_user_id
        assert not result1.error
        assert not result2.error
