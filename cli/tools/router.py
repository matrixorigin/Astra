"""Tool router — dispatches LLM tool_calls to edge tools, executes concurrently."""

import asyncio
import json
from dataclasses import dataclass
from typing import Any

from cli.tools.base import EdgeTool


@dataclass
class ToolCall:
    """A tool call from the LLM."""
    id: str
    name: str
    arguments: dict[str, Any]


@dataclass
class ToolResult:
    """Result of executing a tool call."""
    tool_call_id: str
    name: str
    result: str
    error: bool = False


class ToolRouter:
    """Dispatches tool_calls to registered EdgeTools. Executes independent calls concurrently."""

    def __init__(self) -> None:
        self._tools: dict[str, EdgeTool] = {}

    def register(self, tool: EdgeTool) -> None:
        self._tools[tool.name] = tool

    def get_tool(self, name: str) -> EdgeTool | None:
        return self._tools.get(name)

    def get_schemas(self) -> list[dict[str, Any]]:
        """Return OpenAI-compatible tool schemas for all registered tools."""
        return [t.to_openai_schema() for t in self._tools.values()]

    async def execute(self, tool_calls: list[ToolCall]) -> list[ToolResult]:
        """Execute tool calls concurrently and return results."""
        tasks = [self._execute_one(tc) for tc in tool_calls]
        return await asyncio.gather(*tasks)

    async def _execute_one(self, tc: ToolCall) -> ToolResult:
        tool = self._tools.get(tc.name)
        if not tool:
            return ToolResult(
                tool_call_id=tc.id, name=tc.name,
                result=f"Unknown tool: {tc.name}", error=True,
            )
        try:
            result = await tool.execute(**tc.arguments)
            return ToolResult(tool_call_id=tc.id, name=tc.name, result=result)
        except Exception as e:
            return ToolResult(
                tool_call_id=tc.id, name=tc.name,
                result=f"Error: {type(e).__name__}: {e}", error=True,
            )

    @staticmethod
    def parse_tool_calls(raw: list[dict[str, Any]]) -> list[ToolCall]:
        """Parse raw tool_call dicts from SSE into ToolCall objects."""
        calls = []
        for tc in raw:
            args = tc.get("arguments", {})
            if isinstance(args, str):
                args = json.loads(args)
            calls.append(ToolCall(id=tc["id"], name=tc["name"], arguments=args))
        return calls
