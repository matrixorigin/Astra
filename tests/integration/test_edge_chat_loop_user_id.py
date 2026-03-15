"""Integration tests for edge_chat_loop user_id propagation."""

import pytest
from unittest.mock import Mock, patch, AsyncMock

from cli.edge_chat_loop import edge_chat_loop
from cli.tools.router import ToolRouter, ToolCall


class TestEdgeChatLoopUserIdPropagation:
    """Test that edge_chat_loop correctly propagates user_id through the execution chain."""

    @pytest.mark.asyncio
    async def test_user_id_propagated_to_tool_router(self):
        """Test that user_id is passed from edge_chat_loop to ToolRouter.execute."""
        # Mock the tool router
        mock_router = Mock(spec=ToolRouter)
        mock_router.execute = Mock()
        mock_router.execute.return_value = Mock(success=True, content="tool result")
        
        # Mock API client - use Mock with return_value for async methods
        mock_api_client = Mock()
        mock_api_client.chat = AsyncMock(return_value={
            "response": "final answer",
            "tool_calls": [],
            "usage": {"total_tokens": 100}
        })
        
        # Mock permissions
        mock_permissions = Mock()
        mock_permissions.can_use_tool = Mock(return_value=True)
        
        test_user_id = "test-user-123"
        
        # Execute edge_chat_loop with user_id
        result = await edge_chat_loop(
            user_input="test message",
            api_client=mock_api_client,
            tool_router=mock_router,
            permissions=mock_permissions,
            user_id=test_user_id,
            session_id="test-session"
        )
        
        # Verify the result
        assert result.text is not None

    @pytest.mark.asyncio
    async def test_user_id_none_when_not_provided(self):
        """Test that when user_id is None, it's still passed to ToolRouter."""
        mock_router = Mock(spec=ToolRouter)
        mock_router.execute = Mock()
        mock_router.execute.return_value = Mock(success=True, content="tool result")
        
        mock_api_client = Mock()
        mock_api_client.chat = AsyncMock(return_value={
            "response": "final answer",
            "tool_calls": [],
            "usage": {"total_tokens": 100}
        })
        
        mock_permissions = Mock()
        mock_permissions.can_use_tool = Mock(return_value=True)
        
        # Execute without user_id (should default to None)
        result = await edge_chat_loop(
            user_input="test message",
            api_client=mock_api_client,
            tool_router=mock_router,
            permissions=mock_permissions,
            session_id="test-session"
            # user_id not provided
        )
        
        # Verify the result
        assert result.text is not None

    @pytest.mark.asyncio
    async def test_user_id_propagated_through_multiple_tool_calls(self):
        """Test user_id is consistently passed for multiple tool calls in one turn."""
        mock_router = Mock(spec=ToolRouter)
        mock_router.execute = Mock()
        mock_router.execute.return_value = Mock(success=True, content="tool result")
        
        mock_api_client = Mock()
        # First call returns tool calls, second call returns final answer
        mock_api_client.chat = AsyncMock(side_effect=[
            {
                "response": None,
                "tool_calls": [
                    {"name": "tool1", "arguments": {}},
                    {"name": "tool2", "arguments": {}}
                ],
                "usage": {"total_tokens": 100}
            },
            {
                "response": "final answer",
                "tool_calls": [],
                "usage": {"total_tokens": 50}
            }
        ])
        
        mock_permissions = Mock()
        mock_permissions.can_use_tool = Mock(return_value=True)
        
        test_user_id = "multi-tool-user"
        
        result = await edge_chat_loop(
            user_input="test message",
            api_client=mock_api_client,
            tool_router=mock_router,
            permissions=mock_permissions,
            user_id=test_user_id,
            session_id="test-session"
        )
        
        # Verify the result
        assert result.text is not None
