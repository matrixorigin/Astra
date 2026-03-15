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

from cli.tools.base import EdgeTool
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
        self,
        name: str,
        execution_time_ms: int,
        success: bool,
        error: str | None = None,
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

    async def execute(self, tool_calls: list[ToolCall], user_id: str = None) -> list[ToolResult]:
        """Execute tool calls concurrently and return results."""
        tasks = [self._execute_one(tc, user_id) for tc in tool_calls]
        return await asyncio.gather(*tasks)

    async def _execute_one(self, tc: ToolCall, user_id: str = None) -> ToolResult:
        tool = self._tools.get(tc.name)
        if not tool:
            return ToolResult(
                tool_call_id=tc.id,
                name=tc.name,
                result=f"Unknown tool: {tc.name}",
                error=True,
            )
        # Detect server-side argument parse failures (malformed JSON from LLM).
        if "_parse_error" in tc.arguments:
            return ToolResult(
                tool_call_id=tc.id,
                name=tc.name,
                result=tc.arguments["_parse_error"],
                error=True,
            )
        # Validate required parameters before execution so the LLM gets
        # a clear error instead of a Python TypeError traceback.
        if isinstance(tool, EdgeTool):
            required = tool.parameters.get("required", [])
            missing = [p for p in required if p not in tc.arguments]
            if missing:
                msg = f"Missing required parameter(s): {', '.join(missing)}"
                return ToolResult(
                    tool_call_id=tc.id,
                    name=tc.name,
                    result=msg,
                    error=True,
                )
            # Warn on unknown parameters so LLM gets feedback instead of
            # silent default-value fallback (e.g. dir_path vs path).
            known = set(tool.parameters.get("properties", {}).keys())
            unknown = [k for k in tc.arguments if k not in known]
            if unknown:
                valid = ", ".join(sorted(known)) if known else "(none)"
                msg = f"Unknown parameter(s): {', '.join(unknown)}. Valid: {valid}"
                return ToolResult(
                    tool_call_id=tc.id,
                    name=tc.name,
                    result=msg,
                    error=True,
                )
        t0 = time.monotonic()
        try:
            # EdgeTool: execute(**kwargs) -> str
            # Typed Skill: execute(input: SkillInput) -> SkillOutput
            if (
                hasattr(tool, "_input_cls")
                and tool._input_cls is not None
                and not isinstance(tool, EdgeTool)
            ):
                validated = tool.validate_input(tc.arguments)
                output = await tool.execute(validated)
                # Serialize full output as JSON so LLM sees all fields
                if hasattr(output, "model_dump"):
                    data = output.model_dump(exclude={"cost"}, exclude_none=True)
                    result = json.dumps(data, ensure_ascii=False, default=str)
                else:
                    result = str(output.result) if hasattr(output, "result") else str(output)
            else:
                # Add user_id to kwargs for EdgeTools that need it
                kwargs = tc.arguments.copy()
                if user_id:
                    kwargs["user_id"] = user_id
                result = await tool.execute(**kwargs)
            elapsed = int((time.monotonic() - t0) * 1000)
            if self._hook:
                self._hook.on_tool_executed(tc.name, elapsed, True)
            return ToolResult(
                tool_call_id=tc.id,
                name=tc.name,
                result=result,
                execution_time_ms=elapsed,
            )
        except Exception as e:
            elapsed = int((time.monotonic() - t0) * 1000)
            error_msg = f"Error: {type(e).__name__}: {e}"
            if self._hook:
                self._hook.on_tool_executed(tc.name, elapsed, False, error_msg)
            return ToolResult(
                tool_call_id=tc.id,
                name=tc.name,
                result=error_msg,
                error=True,
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
