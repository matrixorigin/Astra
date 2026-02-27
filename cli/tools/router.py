"""Tool router — dispatches LLM tool_calls to skills, executes concurrently.

All tools are Skills (EdgeTool is a Skill subclass). The router accepts any Skill
and dispatches via the EdgeTool ``execute(**kwargs)`` interface for tools that use it,
or the Pydantic ``execute(input)`` interface for typed skills.
"""

import asyncio
import json
import logging
import time
from dataclasses import dataclass, field
from typing import Any, Protocol

from core.skills.base import Skill

logger = logging.getLogger(__name__)


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
    execution_time_ms: int = 0


class ExecutionHook(Protocol):
    """Optional hook for metrics/audit on each tool execution."""

    def on_tool_executed(
        self, name: str, execution_time_ms: int, success: bool, error: str | None = None,
    ) -> None: ...


class ToolRouter:
    """Dispatches tool_calls to registered Skills. Executes independent calls concurrently."""

    def __init__(self, hook: ExecutionHook | None = None) -> None:
        self._tools: dict[str, Skill] = {}
        self._hook = hook

    def register(self, tool: Skill) -> None:
        self._tools[tool.name] = tool

    def get_tool(self, name: str) -> Skill | None:
        return self._tools.get(name)

    def list_tools(self) -> list[Skill]:
        """Return all registered tools (public API for introspection)."""
        return list(self._tools.values())

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
        t0 = time.monotonic()
        try:
            result = await tool.execute(**tc.arguments)
            elapsed = int((time.monotonic() - t0) * 1000)
            if self._hook:
                self._hook.on_tool_executed(tc.name, elapsed, True)
            return ToolResult(
                tool_call_id=tc.id, name=tc.name, result=result,
                execution_time_ms=elapsed,
            )
        except Exception as e:
            elapsed = int((time.monotonic() - t0) * 1000)
            error_msg = f"Error: {type(e).__name__}: {e}"
            if self._hook:
                self._hook.on_tool_executed(tc.name, elapsed, False, error_msg)
            return ToolResult(
                tool_call_id=tc.id, name=tc.name,
                result=error_msg, error=True,
                execution_time_ms=elapsed,
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
