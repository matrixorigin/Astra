"""Tests for GetAgentInfoTool parameter handling and dimension routing."""

import json

import pytest


class TestGetAgentInfo:
    """Verify get_agent_info handles parameters correctly."""

    @pytest.mark.asyncio
    async def test_valid_dimension_returns_data(self):
        from cli.tools.introspection import GetAgentInfoTool

        tool = GetAgentInfoTool(session_info={"session_id": "s1", "turn": 3})
        result = await tool.execute(dimension="state")
        data = json.loads(result)

        assert "state" in data
        assert data["state"]["session_id"] == "s1"
        assert data["state"]["turn"] == 3

    @pytest.mark.asyncio
    async def test_invalid_dimension_returns_error(self):
        from cli.tools.introspection import GetAgentInfoTool

        tool = GetAgentInfoTool()
        result = await tool.execute(dimension="nonexistent")
        data = json.loads(result)

        assert "error" in data
        assert "nonexistent" in data["error"]

    @pytest.mark.asyncio
    async def test_capability_lists_tools(self):
        """dimension=capability returns tool list from router."""
        from unittest.mock import MagicMock

        from cli.tools.introspection import GetAgentInfoTool

        mock_tool = MagicMock()
        mock_tool.name = "test_tool"
        mock_tool.side_effect = None

        router = MagicMock()
        router.list_tools.return_value = [mock_tool]

        tool = GetAgentInfoTool(tool_router=router)
        result = await tool.execute(dimension="capability")
        data = json.loads(result)

        assert data["capability"]["tool_count"] == 1
        assert data["capability"]["tools"][0]["name"] == "test_tool"

    @pytest.mark.asyncio
    async def test_missing_dimension_uses_default(self):
        """execute() with no dimension arg defaults to 'all'."""
        from cli.tools.introspection import GetAgentInfoTool

        tool = GetAgentInfoTool(session_info={"agent_id": "a1"})
        result = await tool.execute()  # no dimension → default "all"
        data = json.loads(result)

        # "all" includes identity, state, memory, capability
        assert "identity" in data
        assert "state" in data
        assert "memory" in data
        assert "capability" in data

    @pytest.mark.asyncio
    async def test_memory_recall_calls_api(self):
        """dimension=memory_recall calls get_introspection_recall with query."""
        from unittest.mock import AsyncMock

        from cli.tools.introspection import GetAgentInfoTool

        mock_api = AsyncMock()
        mock_api.get_introspection_recall.return_value = {
            "query": "Python async",
            "retrieved_count": 2,
            "ranking": [
                {
                    "rank": 1,
                    "memory_id": "m1",
                    "final_score": 0.8,
                    "scores": {"vector": 0.0, "keyword": 1.0, "temporal": 0.9, "confidence": 0.7},
                },
            ],
        }

        tool = GetAgentInfoTool(
            api_client=mock_api,
            session_info={"session_id": "s1"},
        )
        result = await tool.execute(dimension="memory_recall", query="Python async")
        data = json.loads(result)

        assert "memory_recall" in data
        assert data["memory_recall"]["retrieved_count"] == 2
        assert data["memory_recall"]["ranking"][0]["memory_id"] == "m1"
        mock_api.get_introspection_recall.assert_called_once_with(
            "s1",
            query="Python async",
        )

    @pytest.mark.asyncio
    async def test_memory_recall_requires_query(self):
        """dimension=memory_recall without query returns error."""
        from cli.tools.introspection import GetAgentInfoTool

        tool = GetAgentInfoTool(session_info={"session_id": "s1"})
        result = await tool.execute(dimension="memory_recall")
        data = json.loads(result)

        assert data["memory_recall"]["error"] == "query parameter is required for memory_recall"
