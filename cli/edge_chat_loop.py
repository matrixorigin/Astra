"""Edge Chat Loop — drives the agentic turn loop between edge and cloud.

Edge sends user message + tool results → cloud returns text + tool_calls → edge executes tools → repeat.
"""

import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

from cli.permissions import Decision, PermissionManager
from cli.tools.router import ToolCall, ToolResult, ToolRouter

MAX_TURNS = 25


class Renderer(Protocol):
    """Pluggable output renderer (terminal, file, test stub)."""

    def text(self, chunk: str) -> None: ...
    def tool_start(self, name: str, args: dict[str, Any]) -> None: ...
    def tool_done(self, name: str, result: str, error: bool) -> None: ...
    def error(self, msg: str) -> None: ...
    def info(self, msg: str) -> None: ...


class StderrRenderer:
    """Default renderer: text to stdout, status to stderr."""

    def text(self, chunk: str) -> None:
        sys.stdout.write(chunk)
        sys.stdout.flush()

    def tool_start(self, name: str, args: dict[str, Any]) -> None:
        detail = args.get("command", args.get("path", ""))
        sys.stderr.write(f"\n  🔧 {name}: {detail}… ")
        sys.stderr.flush()

    def tool_done(self, name: str, result: str, error: bool) -> None:
        sys.stderr.write("✗\n" if error else "✓\n")
        sys.stderr.flush()

    def error(self, msg: str) -> None:
        sys.stderr.write(f"\n❌ {msg}\n")
        sys.stderr.flush()

    def info(self, msg: str) -> None:
        sys.stderr.write(f"{msg}\n")
        sys.stderr.flush()


@dataclass
class TurnResult:
    """Parsed result of one /chat/turn SSE stream."""

    text: str = ""
    tool_calls: list[dict[str, Any]] = field(default_factory=list)
    session_id: str | None = None
    run_id: str | None = None
    usage: dict[str, int] = field(default_factory=dict)
    has_tool_calls: bool = False


def load_project_rules(project_root: str) -> str | None:
    """Load project rules from local config files."""
    root = Path(project_root)
    candidates = [
        root / ".mo-agent" / "rules.md",
        root / "CLAUDE.md",  # compatibility
    ]
    # Also load steering files
    steering_dir = root / ".mo-agent" / "steering"
    if steering_dir.is_dir():
        candidates.extend(sorted(steering_dir.glob("*.md")))

    parts = []
    for p in candidates:
        if p.is_file():
            try:
                parts.append(f"# {p.name}\n{p.read_text().strip()}")
            except OSError:
                pass
    return "\n\n---\n\n".join(parts) if parts else None


async def _consume_turn(sse_stream, renderer: Renderer) -> TurnResult:
    """Consume one /chat/turn SSE stream, render text, collect tool_calls."""
    result = TurnResult()
    async for event in sse_stream:
        etype = event.get("type", "")
        if etype == "text_delta":
            chunk = event.get("content", "")
            result.text += chunk
            renderer.text(chunk)
        elif etype == "tool_call":
            result.tool_calls.append(event)
        elif etype == "session_info":
            result.session_id = event.get("session_id")
            result.run_id = event.get("run_id")
        elif etype == "usage":
            result.usage = {k: v for k, v in event.items() if k != "type"}
        elif etype == "turn_complete":
            result.has_tool_calls = event.get("has_tool_calls", False)
        elif etype == "error":
            renderer.error(event.get("message", "Unknown cloud error"))
    return result


async def edge_chat_loop(
    user_input: str,
    api_client: Any,
    tool_router: ToolRouter,
    permissions: PermissionManager,
    *,
    session_id: str | None = None,
    project_root: str = ".",
    renderer: Renderer | None = None,
    agent_id: str | None = None,
    model: str | None = None,
) -> str:
    """Run the edge-cloud agentic loop until final answer or MAX_TURNS.

    Returns the final assistant text.
    """
    renderer = renderer or StderrRenderer()
    messages: list[dict[str, Any]] = [{"role": "user", "content": user_input}]
    tool_results: list[dict[str, Any]] = []
    project_rules = load_project_rules(project_root)
    final_text = ""

    for turn in range(MAX_TURNS):
        # Call cloud
        try:
            sse_stream = api_client.chat_turn(
                messages=messages,
                session_id=session_id,
                tool_results=tool_results if tool_results else None,
                project_rules=project_rules if turn == 0 else None,
                agent_id=agent_id,
                model=model,
                edge_tools=tool_router.get_schemas() if turn == 0 else None,
            )
            result = await _consume_turn(sse_stream, renderer)
        except KeyboardInterrupt:
            renderer.error("Interrupted by user")
            break
        except (ConnectionError, OSError, TimeoutError) as e:
            renderer.error(f"Network error: {e}")
            break
        except Exception as e:
            renderer.error(f"{type(e).__name__}: {e}")
            break

        # Track session from first response
        if result.session_id and not session_id:
            session_id = result.session_id

        final_text = result.text

        if not result.has_tool_calls:
            break

        # Execute tool calls locally
        parsed = ToolRouter.parse_tool_calls(result.tool_calls)
        approved: list[ToolCall] = []
        tool_results = []

        for tc in parsed:
            tool = tool_router.get_tool(tc.name)
            side_effect = tool.side_effect if tool else None

            if side_effect is None:
                tool_results.append({"tool_call_id": tc.id, "name": tc.name, "result": f"Unknown tool: {tc.name}"})
                continue

            decision = permissions.check(tc.name, side_effect, tc.arguments)

            if decision == Decision.DENY:
                renderer.tool_start(tc.name, tc.arguments)
                renderer.tool_done(tc.name, "Blocked (dangerous)", True)
                tool_results.append({"tool_call_id": tc.id, "name": tc.name, "result": "Permission denied: command blocked by safety policy"})
                continue

            if decision == Decision.ASK:
                decision = permissions.prompt_user(tc.name, side_effect, tc.arguments)
                if decision == Decision.DENY:
                    renderer.tool_done(tc.name, "Denied by user", True)
                    tool_results.append({"tool_call_id": tc.id, "name": tc.name, "result": "Permission denied by user"})
                    continue

            approved.append(tc)

        # Execute approved tools concurrently
        if approved:
            for tc in approved:
                renderer.tool_start(tc.name, tc.arguments)
            results = await tool_router.execute(approved)
            for r in results:
                renderer.tool_done(r.name, r.result, r.error)
                tool_results.append({"tool_call_id": r.tool_call_id, "name": r.name, "result": r.result})

        # Clear messages after first turn — cloud has the history
        messages = []
    else:
        renderer.error(f"Reached maximum turns ({MAX_TURNS})")

    return final_text
