"""Integration tests for memory_program tool with real user ID propagation."""

import json
import pytest
from unittest.mock import Mock, patch

from cli.tools.router import ToolRouter, ToolCall
from cli.tools.memory_program import MemoryProgramTool


class TestMemoryProgramToolIntegration:
    """Test memory_program tool with real user ID propagation."""

    @pytest.mark.asyncio
    async def test_user_id_propagation_to_memoria_editor(self):
        """Test that user_id is correctly passed to memoria editor."""
        router = ToolRouter()
        router.register(MemoryProgramTool())
        
        # Mock the editor creation to verify user_id
        with patch('core.memory.factory.create_editor') as mock_create_editor:
            mock_editor = Mock()
            mock_editor.inject.return_value = {"memory_id": "test-123"}
            mock_create_editor.return_value = mock_editor
            
            # Execute with specific user_id
            test_user_id = "user-abc-123"
            call = ToolCall(
                id="call-1",
                name="memory_program",
                arguments={
                    "actions": [{
                        "operation": "inject",
                        "content": "Test memory content",
                        "memory_type": "semantic"
                    }]
                }
            )
            
            results = await router.execute([call], user_id=test_user_id)
            result = results[0]
            
            # Verify editor was created with correct user_id
            mock_create_editor.assert_called_once_with(None, user_id=test_user_id)
            
            # Verify inject was called with session_id=None (no session in this call)
            mock_editor.inject.assert_called_once_with(
                content="Test memory content",
                memory_type="semantic",
                source="memory_program_tool",
                session_id=None,
            )
            
            # Verify result
            assert not result.error
            result_data = json.loads(result.result)
            assert result_data["status"] == "success"
            assert result_data["actions_executed"] == 1

    @pytest.mark.asyncio
    async def test_default_user_id_fallback(self):
        """Test fallback to 'default' when no user_id provided."""
        router = ToolRouter()
        router.register(MemoryProgramTool())
        
        with patch('core.memory.factory.create_editor') as mock_create_editor:
            mock_editor = Mock()
            mock_editor.inject.return_value = {"memory_id": "test-123"}
            mock_create_editor.return_value = mock_editor
            
            call = ToolCall(
                id="call-2",
                name="memory_program",
                arguments={
                    "actions": [{
                        "operation": "inject",
                        "content": "Test content"
                    }]
                }
            )
            
            # Execute without user_id
            results = await router.execute([call])
            result = results[0]
            
            # Should fallback to "default"
            mock_create_editor.assert_called_once_with(None, user_id="default")
