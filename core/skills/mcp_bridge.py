"""MCP Bridge: connect external MCP servers to mo-agent-engine.

Converts MCP tools to OpenAI function calling schema so ChatLoop can
use them alongside built-in skills. Supports stdio and streamable HTTP
transports.

Usage:
    bridge = MCPBridge()
    await bridge.connect_stdio("filesystem", "npx", ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"])
    await bridge.connect_http("remote-api", "http://localhost:8000/mcp")

    tools = await bridge.get_tools_schema()  # OpenAI function calling format
    result = await bridge.call_tool("read_file", {"path": "/tmp/test.txt"})

    await bridge.close()
"""

import json
import logging
from contextlib import AsyncExitStack
from typing import Any

from mcp import ClientSession
from mcp.client.stdio import stdio_client
from mcp.client.streamable_http import streamablehttp_client

logger = logging.getLogger(__name__)


class MCPServerHandle:
    """A connected MCP server with its session and metadata."""

    __slots__ = ("name", "session", "tools", "transport")

    def __init__(self, name: str, session: ClientSession, transport: str):
        self.name = name
        self.session = session
        self.transport = transport
        self.tools: list[dict[str, Any]] = []


class MCPBridge:
    """Bridge between MCP servers and ChatLoop's function calling interface.

    Manages multiple MCP server connections. Each MCP tool is exposed as an
    OpenAI function calling schema with a namespaced name (server__tool) to
    avoid collisions.
    """

    def __init__(self):
        self._servers: dict[str, MCPServerHandle] = {}
        self._tool_to_server: dict[str, str] = {}  # tool_name → server_name
        self._exit_stack = AsyncExitStack()

    async def connect_stdio(
        self, name: str, command: str, args: list[str] | None = None,
        env: dict[str, str] | None = None,
    ) -> int:
        """Connect to an MCP server via stdio (subprocess).

        Returns number of tools discovered.
        """
        read_stream, write_stream = await self._exit_stack.enter_async_context(
            stdio_client(command=command, args=args or [], env=env)
        )
        session = await self._exit_stack.enter_async_context(
            ClientSession(read_stream, write_stream)
        )
        await session.initialize()
        return await self._register_server(name, session, "stdio")

    async def connect_http(
        self, name: str, url: str, headers: dict[str, str] | None = None,
    ) -> int:
        """Connect to an MCP server via streamable HTTP.

        Returns number of tools discovered.
        """
        read_stream, write_stream, _ = await self._exit_stack.enter_async_context(
            streamablehttp_client(url=url, headers=headers)
        )
        session = await self._exit_stack.enter_async_context(
            ClientSession(read_stream, write_stream)
        )
        await session.initialize()
        return await self._register_server(name, session, "streamable_http")

    async def _register_server(
        self, name: str, session: ClientSession, transport: str,
    ) -> int:
        """Register a connected server and discover its tools."""
        handle = MCPServerHandle(name, session, transport)

        result = await session.list_tools()
        for tool in result.tools:
            # Namespace: server__tool to avoid collisions
            qualified_name = f"{name}__{tool.name}"
            schema = self._mcp_tool_to_openai_schema(qualified_name, tool)
            handle.tools.append(schema)
            self._tool_to_server[qualified_name] = name

        self._servers[name] = handle
        logger.info(f"MCP: connected to '{name}' ({transport}), {len(handle.tools)} tools")
        return len(handle.tools)

    def _mcp_tool_to_openai_schema(self, name: str, tool) -> dict[str, Any]:
        """Convert MCP Tool to OpenAI function calling schema."""
        parameters = tool.inputSchema or {"type": "object", "properties": {}}
        # Ensure 'type' is present
        if "type" not in parameters:
            parameters["type"] = "object"
        return {
            "type": "function",
            "function": {
                "name": name,
                "description": tool.description or f"MCP tool: {tool.name}",
                "parameters": parameters,
            },
        }

    async def get_tools_schema(self) -> list[dict[str, Any]]:
        """Get all MCP tools in OpenAI function calling format."""
        tools = []
        for handle in self._servers.values():
            tools.extend(handle.tools)
        return tools

    def is_mcp_tool(self, tool_name: str) -> bool:
        """Check if a tool name belongs to an MCP server."""
        return tool_name in self._tool_to_server

    async def call_tool(
        self, tool_name: str, arguments: dict[str, Any] | None = None,
    ) -> str:
        """Call an MCP tool and return the result as a string.

        Args:
            tool_name: Qualified name (server__tool)
            arguments: Tool arguments

        Returns:
            Tool result as JSON string
        """
        server_name = self._tool_to_server.get(tool_name)
        if not server_name:
            return json.dumps({"error": f"Unknown MCP tool: {tool_name}"})

        handle = self._servers[server_name]
        # Strip namespace prefix to get original MCP tool name
        original_name = tool_name[len(server_name) + 2:]  # skip "name__"

        try:
            result = await handle.session.call_tool(original_name, arguments)

            # Extract text content from MCP result
            parts = []
            for content in result.content:
                if hasattr(content, "text"):
                    parts.append(content.text)
                elif hasattr(content, "data"):
                    parts.append(f"[binary: {content.mimeType}]")
                else:
                    parts.append(str(content))

            output = "\n".join(parts) if parts else ""

            if result.isError:
                return json.dumps({"error": output})
            return output if output else json.dumps({"result": "ok"})

        except Exception as e:
            logger.error(f"MCP tool '{tool_name}' failed: {e}")
            return json.dumps({"error": str(e)})

    @property
    def server_names(self) -> list[str]:
        """List connected server names."""
        return list(self._servers.keys())

    @property
    def tool_count(self) -> int:
        """Total number of MCP tools across all servers."""
        return len(self._tool_to_server)

    async def refresh_tools(self, server_name: str | None = None) -> int:
        """Re-fetch tool list from server(s). Returns total tool count."""
        targets = [server_name] if server_name else list(self._servers.keys())
        total = 0
        for name in targets:
            handle = self._servers.get(name)
            if not handle:
                continue
            # Clear old mappings
            for tool in handle.tools:
                self._tool_to_server.pop(tool["function"]["name"], None)
            handle.tools.clear()

            result = await handle.session.list_tools()
            for tool in result.tools:
                qualified_name = f"{name}__{tool.name}"
                schema = self._mcp_tool_to_openai_schema(qualified_name, tool)
                handle.tools.append(schema)
                self._tool_to_server[qualified_name] = name
            total += len(handle.tools)
        return total

    async def close(self):
        """Close all MCP server connections."""
        await self._exit_stack.aclose()
        self._servers.clear()
        self._tool_to_server.clear()
        logger.info("MCP: all connections closed")
