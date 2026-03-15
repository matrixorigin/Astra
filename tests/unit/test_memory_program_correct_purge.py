"""Tests for memory_program correct/purge operations and topic-based purge."""

from __future__ import annotations

import json
import pytest
from unittest.mock import Mock, patch


@pytest.mark.asyncio
class TestMemoryProgramCorrectPurge:
    async def _run(self, actions, user_id="u1", session_id=None):
        from cli.tools.router import ToolRouter, ToolCall
        from cli.tools.memory_program import MemoryProgramTool

        router = ToolRouter()
        router.register(MemoryProgramTool())

        with patch("core.memory.factory.create_editor") as mock_create:
            mock_editor = Mock()
            mock_editor.inject.return_value = {"memory_id": "m1"}
            mock_editor.correct.return_value = {"memory_id": "m1"}
            mock_editor.purge.return_value = Mock(deactivated=1)
            mock_create.return_value = mock_editor

            call = ToolCall(id="c1", name="memory_program", arguments={"actions": actions})
            results = await router.execute([call], user_id=user_id, session_id=session_id)
            return json.loads(results[0].result), mock_editor

    async def test_correct_calls_editor_correct(self):
        result, editor = await self._run(
            [
                {
                    "operation": "correct",
                    "memory_id": "mem-abc",
                    "content": "updated content",
                    "reason": "user corrected",
                }
            ]
        )
        assert result["status"] == "success"
        editor.correct.assert_called_once_with("mem-abc", "updated content", "user corrected")

    async def test_correct_requires_memory_id(self):
        result, editor = await self._run(
            [
                {
                    "operation": "correct",
                    "content": "new content",
                }
            ]
        )
        assert result["results"][0]["status"] == "error"
        assert "memory_id" in result["results"][0]["error"]
        editor.correct.assert_not_called()

    async def test_correct_requires_content(self):
        result, editor = await self._run(
            [
                {
                    "operation": "correct",
                    "memory_id": "mem-abc",
                }
            ]
        )
        assert result["results"][0]["status"] == "error"
        assert "content" in result["results"][0]["error"]
        editor.correct.assert_not_called()

    async def test_purge_by_id(self):
        result, editor = await self._run(
            [
                {
                    "operation": "purge",
                    "memory_id": "mem-xyz",
                    "reason": "done",
                }
            ]
        )
        assert result["status"] == "success"
        editor.purge.assert_called_once_with(memory_id="mem-xyz", topic=None, reason="done")

    async def test_purge_by_topic(self):
        """Topic-based bulk purge must be supported."""
        result, editor = await self._run(
            [
                {
                    "operation": "purge",
                    "topic": "old project",
                    "reason": "cleanup",
                }
            ]
        )
        assert result["status"] == "success"
        editor.purge.assert_called_once_with(memory_id=None, topic="old project", reason="cleanup")

    async def test_purge_requires_id_or_topic(self):
        result, editor = await self._run([{"operation": "purge"}])
        assert result["results"][0]["status"] == "error"
        assert "memory_id or topic" in result["results"][0]["error"]
        editor.purge.assert_not_called()
