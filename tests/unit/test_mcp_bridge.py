"""Unit tests for MCP Bridge."""

import json
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from core.skills.mcp_bridge import MCPBridge, MCPServerHandle


@pytest.fixture
def bridge():
    return MCPBridge()


def _mock_tool(name="read_file", description="Read a file", input_schema=None):
    """Create a mock MCP Tool object."""
    tool = MagicMock()
    tool.name = name
    tool.description = description
    tool.inputSchema = input_schema or {
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
    }
    return tool


class TestMCPToolConversion:
    def test_mcp_tool_to_openai_schema(self, bridge):
        tool = _mock_tool()
        schema = bridge._mcp_tool_to_openai_schema("fs__read_file", tool)

        assert schema["type"] == "function"
        assert schema["function"]["name"] == "fs__read_file"
        assert schema["function"]["description"] == "Read a file"
        assert schema["function"]["parameters"]["type"] == "object"
        assert "path" in schema["function"]["parameters"]["properties"]

    def test_mcp_tool_missing_type(self, bridge):
        """inputSchema without 'type' gets 'object' added."""
        tool = _mock_tool(input_schema={"properties": {"x": {"type": "string"}}})
        schema = bridge._mcp_tool_to_openai_schema("s__t", tool)
        assert schema["function"]["parameters"]["type"] == "object"

    def test_mcp_tool_no_schema(self, bridge):
        tool = _mock_tool(input_schema=None)
        tool.inputSchema = None
        schema = bridge._mcp_tool_to_openai_schema("s__t", tool)
        assert schema["function"]["parameters"]["type"] == "object"


class TestMCPBridgeRegistration:
    @pytest.mark.asyncio
    async def test_register_server(self, bridge):
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("read"), _mock_tool("write")]
        session.list_tools.return_value = list_result

        count = await bridge._register_server("fs", session, "stdio")

        assert count == 2
        assert bridge.tool_count == 2
        assert "fs" in bridge.server_names
        assert bridge.is_mcp_tool("fs__read")
        assert bridge.is_mcp_tool("fs__write")
        assert not bridge.is_mcp_tool("unknown__tool")

    @pytest.mark.asyncio
    async def test_get_tools_schema(self, bridge):
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("search"), _mock_tool("index")]
        session.list_tools.return_value = list_result

        await bridge._register_server("db", session, "http")
        tools = await bridge.get_tools_schema()

        assert len(tools) == 2
        names = [t["function"]["name"] for t in tools]
        assert "db__search" in names
        assert "db__index" in names

    @pytest.mark.asyncio
    async def test_multiple_servers(self, bridge):
        for name, tool_names in [("fs", ["read", "write"]), ("db", ["query"])]:
            session = AsyncMock()
            result = MagicMock()
            result.tools = [_mock_tool(t) for t in tool_names]
            session.list_tools.return_value = result
            await bridge._register_server(name, session, "stdio")

        assert bridge.tool_count == 3
        assert len(bridge.server_names) == 2
        tools = await bridge.get_tools_schema()
        assert len(tools) == 3


class TestMCPBridgeCallTool:
    @pytest.mark.asyncio
    async def test_call_tool_success(self, bridge):
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("read_file")]
        session.list_tools.return_value = list_result

        # Mock call_tool result
        content_item = MagicMock()
        content_item.text = "file contents here"
        del content_item.data  # Ensure hasattr(content, 'data') is False
        call_result = MagicMock()
        call_result.content = [content_item]
        call_result.isError = False
        session.call_tool.return_value = call_result

        await bridge._register_server("fs", session, "stdio")
        result = await bridge.call_tool("fs__read_file", {"path": "/tmp/test"})

        assert result == "file contents here"
        session.call_tool.assert_called_once_with("read_file", {"path": "/tmp/test"})

    @pytest.mark.asyncio
    async def test_call_tool_error_result(self, bridge):
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("delete")]
        session.list_tools.return_value = list_result

        content_item = MagicMock()
        content_item.text = "permission denied"
        del content_item.data
        call_result = MagicMock()
        call_result.content = [content_item]
        call_result.isError = True
        session.call_tool.return_value = call_result

        await bridge._register_server("fs", session, "stdio")
        result = await bridge.call_tool("fs__delete", {"path": "/etc/passwd"})

        parsed = json.loads(result)
        assert "error" in parsed
        assert "permission denied" in parsed["error"]

    @pytest.mark.asyncio
    async def test_call_tool_exception(self, bridge):
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("crash")]
        session.list_tools.return_value = list_result
        session.call_tool.side_effect = RuntimeError("connection lost")

        await bridge._register_server("s", session, "stdio")
        result = await bridge.call_tool("s__crash", {})

        parsed = json.loads(result)
        assert "connection lost" in parsed["error"]

    @pytest.mark.asyncio
    async def test_call_unknown_tool(self, bridge):
        result = await bridge.call_tool("nonexistent__tool", {})
        parsed = json.loads(result)
        assert "Unknown MCP tool" in parsed["error"]


class TestMCPBridgeRefresh:
    @pytest.mark.asyncio
    async def test_refresh_tools(self, bridge):
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("old_tool")]
        session.list_tools.return_value = list_result

        await bridge._register_server("s", session, "stdio")
        assert bridge.is_mcp_tool("s__old_tool")

        # Server now has different tools
        new_result = MagicMock()
        new_result.tools = [_mock_tool("new_tool")]
        session.list_tools.return_value = new_result

        total = await bridge.refresh_tools("s")
        assert total == 1
        assert not bridge.is_mcp_tool("s__old_tool")
        assert bridge.is_mcp_tool("s__new_tool")


class TestChatLoopMCPIntegration:
    def _make_loop(self):
        from core.agent.chat_loop import ChatLoop
        return ChatLoop(
            selector=MagicMock(),
            executor=MagicMock(),
            llm_client=MagicMock(),
            event_logger=MagicMock(),
            context_manager=MagicMock(),
            firewall=MagicMock(),
        )

    def test_set_mcp_bridge(self):
        loop = self._make_loop()
        assert loop.mcp_bridge is None
        bridge = MagicMock()
        loop.set_mcp_bridge(bridge)
        assert loop.mcp_bridge is bridge


class TestMCPRetry:
    @pytest.mark.asyncio
    async def test_call_tool_retries_on_connection_error(self, bridge):
        """Transient ConnectionError should be retried."""
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("flaky")]
        session.list_tools.return_value = list_result

        # Fail twice with ConnectionError, succeed on third
        content_item = MagicMock()
        content_item.text = "ok"
        del content_item.data
        success = MagicMock(content=[content_item], isError=False)
        session.call_tool.side_effect = [ConnectionError("reset"), ConnectionError("reset"), success]

        await bridge._register_server("s", session, "stdio")
        result = await bridge.call_tool("s__flaky", {})

        assert result == "ok"
        assert session.call_tool.call_count == 3

    @pytest.mark.asyncio
    async def test_call_tool_exhausts_retries(self, bridge):
        """After MAX_RETRIES+1 attempts, returns error."""
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("dead")]
        session.list_tools.return_value = list_result
        session.call_tool.side_effect = ConnectionError("gone")

        await bridge._register_server("s", session, "stdio")
        result = await bridge.call_tool("s__dead", {})

        parsed = json.loads(result)
        assert "gone" in parsed["error"]

    @pytest.mark.asyncio
    async def test_non_transient_error_no_retry(self, bridge):
        """Non-connection errors should not be retried."""
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("bad")]
        session.list_tools.return_value = list_result
        session.call_tool.side_effect = ValueError("bad args")

        await bridge._register_server("s", session, "stdio")
        result = await bridge.call_tool("s__bad", {})

        parsed = json.loads(result)
        assert "bad args" in parsed["error"]
        assert session.call_tool.call_count == 1


class TestMCPToolMetadata:
    @pytest.mark.asyncio
    async def test_tool_metadata_list(self, bridge):
        """tool_metadata_list returns lightweight metadata for registry integration."""
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("read"), _mock_tool("write")]
        session.list_tools.return_value = list_result

        await bridge._register_server("fs", session, "stdio")
        meta = bridge.tool_metadata_list()

        assert len(meta) == 2
        assert meta[0]["name"] == "fs__read"
        assert meta[0]["category"] == "mcp"
        assert meta[0]["version"] == "mcp:stdio"
        assert meta[0]["server"] == "fs"


class TestMCPToolsChangedCallback:
    @pytest.mark.asyncio
    async def test_on_tools_changed_fires_on_register(self, bridge):
        callback = MagicMock()
        bridge.set_on_tools_changed(callback)

        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("t")]
        session.list_tools.return_value = list_result

        await bridge._register_server("s", session, "stdio")
        callback.assert_called_once()

    @pytest.mark.asyncio
    async def test_on_tools_changed_fires_on_refresh(self, bridge):
        session = AsyncMock()
        list_result = MagicMock()
        list_result.tools = [_mock_tool("t")]
        session.list_tools.return_value = list_result
        await bridge._register_server("s", session, "stdio")

        callback = MagicMock()
        bridge.set_on_tools_changed(callback)

        await bridge.refresh_tools("s")
        callback.assert_called_once()

    @pytest.mark.asyncio
    async def test_on_tools_changed_fires_on_close(self, bridge):
        callback = MagicMock()
        bridge.set_on_tools_changed(callback)
        await bridge.close()
        callback.assert_called_once()


