"""Tests for MCP-style edge memory tools."""

from __future__ import annotations

import asyncio
import json
from unittest.mock import MagicMock, patch

from core.memory.types import Memory, MemoryType


def _make_memory(memory_id: str = "m1", content: str = "prefers vim") -> Memory:
    return Memory(memory_id=memory_id, user_id="alice", content=content, memory_type=MemoryType.SEMANTIC)


class TestMemoryRetrieveTool:
    def test_retrieve_returns_results_and_explain(self):
        from cli.tools.memory_mcp import MemoryRetrieveTool

        tool = MemoryRetrieveTool()
        svc = MagicMock()
        svc.retrieve.return_value = ([_make_memory()], {"source": "memoria", "final_count": 1})

        with patch("core.memory.backends.get_memoria_storage", return_value=svc):
            result = asyncio.run(
                tool.execute("editor preference", top_k=3, session_id="s1", user_id="alice")
            )

        data = json.loads(result)
        assert data["results"][0]["memory_id"] == "m1"
        assert data["explain"]["final_count"] == 1
        svc.retrieve.assert_called_once_with(
            user_id="alice",
            query="editor preference",
            top_k=3,
            session_id="s1",
            explain=False,
        )


class TestMemorySearchTool:
    def test_search_uses_client_search(self):
        from cli.tools.memory_mcp import MemorySearchTool

        tool = MemorySearchTool()
        svc = MagicMock()
        svc.client.search.return_value = [{"memory_id": "m1", "content": "pytest -n auto"}]

        with patch("core.memory.backends.get_memoria_storage", return_value=svc):
            result = asyncio.run(tool.execute("pytest", top_k=2, user_id="alice"))

        data = json.loads(result)
        assert data["results"][0]["memory_id"] == "m1"
        svc.client.search.assert_called_once_with(user_id="alice", query="pytest", top_k=2)


class TestMemoryProfileTool:
    def test_profile_returns_client_payload(self):
        from cli.tools.memory_mcp import MemoryProfileTool

        tool = MemoryProfileTool()
        svc = MagicMock()
        svc.client.get_profile.return_value = {"profile": "User prefers concise answers", "stats": {}}

        with patch("core.memory.backends.get_memoria_storage", return_value=svc):
            result = asyncio.run(tool.execute(user_id="alice"))

        data = json.loads(result)
        assert "concise answers" in data["profile"]
        svc.client.get_profile.assert_called_once_with("alice")


class TestMemoryStoreTool:
    def test_store_calls_storage_store(self):
        from cli.tools.memory_mcp import MemoryStoreTool

        tool = MemoryStoreTool()
        svc = MagicMock()
        svc.store.return_value = _make_memory(content="Uses pytest")

        with patch("core.memory.backends.get_memoria_storage", return_value=svc):
            result = asyncio.run(
                tool.execute(
                    content="Uses pytest",
                    memory_type="procedural",
                    session_id="sess-1",
                    user_id="alice",
                )
            )

        data = json.loads(result)
        assert data["content"] == "Uses pytest"
        kwargs = svc.store.call_args.kwargs
        assert kwargs["user_id"] == "alice"
        assert kwargs["memory_type"] == MemoryType.PROCEDURAL
        assert kwargs["session_id"] == "sess-1"


class TestMemoryCorrectTool:
    def test_correct_by_query_uses_client_api(self):
        from cli.tools.memory_mcp import MemoryCorrectTool

        tool = MemoryCorrectTool()
        svc = MagicMock()
        svc.client.correct_by_query.return_value = {"updated": 1}

        with patch("core.memory.backends.get_memoria_storage", return_value=svc):
            result = asyncio.run(
                tool.execute(
                    query="editor preference",
                    new_content="Prefers neovim",
                    reason="updated",
                    user_id="alice",
                )
            )

        data = json.loads(result)
        assert data["updated"] == 1
        svc.client.correct_by_query.assert_called_once_with(
            user_id="alice",
            query="editor preference",
            new_content="Prefers neovim",
            reason="updated",
        )


class TestMemoryPurgeTool:
    def test_purge_splits_comma_separated_ids(self):
        from cli.tools.memory_mcp import MemoryPurgeTool

        tool = MemoryPurgeTool()
        svc = MagicMock()
        svc.purge.return_value = type("PurgeResult", (), {"deactivated": 2})()

        with patch("core.memory.backends.get_memoria_storage", return_value=svc):
            result = asyncio.run(
                tool.execute(memory_id="m1, m2", reason="cleanup", user_id="alice")
            )

        data = json.loads(result)
        assert data["purged"] == 2
        kwargs = svc.purge.call_args.kwargs
        assert kwargs["memory_ids"] == ["m1", "m2"]
        assert kwargs["reason"] == "cleanup"


class TestMemoryToolRegistration:
    def test_register_respects_backend_capabilities(self):
        from cli.tools.memory_mcp import register_memory_mcp_tools
        from core.memory.backends.factory import MemoryBackendCapabilities

        router = MagicMock()
        registered = []
        router.register.side_effect = lambda tool: registered.append(tool.name)

        capabilities = MemoryBackendCapabilities(
            backend_name="test",
            supported_tools=("memory_retrieve", "memory_profile"),
            supported_context_modes=("profile_only", "retrieve"),
        )

        with patch(
            "core.memory.backends.get_memory_backend_capabilities",
            return_value=capabilities,
        ):
            register_memory_mcp_tools(router)

        assert registered == ["memory_retrieve", "memory_profile"]
