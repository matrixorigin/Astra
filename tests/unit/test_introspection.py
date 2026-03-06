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
