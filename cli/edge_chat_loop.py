"""Edge Chat Loop — drives the agentic turn loop between edge and cloud.

Edge sends user message + tool results → cloud returns text + tool_calls → edge executes tools → repeat.
"""

import asyncio
import sys
from dataclasses import dataclass, field
from itertools import islice
from pathlib import Path
from typing import Any, Protocol

import httpx

from cli.api_client import AuthenticationError
from cli.permissions import Decision, PermissionManager
from cli.tools.base import resolve_side_effect
from cli.tools.router import ToolCall, ToolRouter

MAX_TURNS = 25
MAX_TURN_WALL_CLOCK_S = 300


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
    error: dict[str, Any] | None = None


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


def detect_edge_profile(project_root: str) -> dict[str, Any]:
    """Detect project profile from local filesystem for cloud context enrichment."""
    import subprocess
    root = Path(project_root).resolve()
    profile: dict[str, Any] = {"cwd": str(root)}

    # Git branch — use symbolic-ref (faster than rev-parse on large repos)
    try:
        branch = subprocess.run(
            ["git", "symbolic-ref", "--short", "HEAD"],
            cwd=root, capture_output=True, text=True, timeout=5,
        )
        if branch.returncode == 0:
            profile["git_branch"] = branch.stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        # OSError covers FileNotFoundError (git not installed) since Python 3.3
        pass

    # Project type from marker files
    markers = {
        "go.mod": "go", "Cargo.toml": "rust", "package.json": "node",
        "pyproject.toml": "python", "pom.xml": "java", "build.gradle": "java",
    }
    for marker, ptype in markers.items():
        if (root / marker).exists():
            profile["project_type"] = ptype
            break

    # Languages from file extensions (sample top-level + common subdirs)
    exts: set[str] = set()
    for d in [root, root / "src", root / "pkg", root / "lib",
              root / "cmd", root / "internal", root / "app"]:
        if d.is_dir():
            for f in islice(d.iterdir(), 50):
                if f.is_file() and f.suffix:
                    exts.add(f.suffix.lstrip("."))
    lang_map = {"go": "Go", "rs": "Rust", "py": "Python", "ts": "TypeScript",
                "js": "JavaScript", "java": "Java", "rb": "Ruby", "c": "C", "cpp": "C++"}
    langs = sorted({lang_map[e] for e in exts if e in lang_map})
    if langs:
        profile["languages"] = langs

    return profile


async def _consume_turn(
    sse_stream,
    renderer: Renderer,
    *,
    timeout: float = MAX_TURN_WALL_CLOCK_S,
    suppress_prefix: str = "",
) -> TurnResult:
    """Consume one /chat/turn SSE stream, render text, collect tool_calls.

    suppress_prefix: if the LLM repeats the previous turn's text verbatim
    at the start of this turn, skip rendering the duplicate prefix.
    """
    result = TurnResult()
    # Dedup state: accumulate early chunks until we can decide whether
    # the LLM is repeating the previous turn's preamble.
    _dedup_buf = "" if suppress_prefix else None  # None = dedup disabled
    _dedup_prefix = suppress_prefix.strip()

    def _flush_dedup():
        """Flush any buffered dedup text to the renderer."""
        nonlocal _dedup_buf
        if _dedup_buf is not None and _dedup_buf:
            renderer.text(_dedup_buf)
        _dedup_buf = None

    deadline = asyncio.timeout(timeout)
    try:
        async with deadline:
            async for event in sse_stream:
                etype = event.get("type", "")
                if etype == "text_delta":
                    chunk = event.get("content", "")
                    result.text += chunk
                    # Dedup: buffer early chunks and check if they repeat
                    # the previous turn's preamble text verbatim.
                    if _dedup_buf is not None:
                        _dedup_buf += chunk
                        buf_s = _dedup_buf.strip()
                        if not buf_s:
                            continue  # only whitespace so far
                        if _dedup_prefix.startswith(buf_s):
                            if buf_s == _dedup_prefix:
                                _dedup_buf = None  # full match — suppressed
                            continue  # partial match — keep buffering
                        # Not a prefix match — flush buffer and disable dedup
                        renderer.text(_dedup_buf)
                        _dedup_buf = None
                        continue
                    renderer.text(chunk)
                elif etype == "tool_call_start":
                    _flush_dedup()
                    # LLM started generating a tool call — show which tool
                    # so user sees progress instead of just "Thinking…"
                    name = event.get("name", "")
                    if hasattr(renderer, "thinking"):
                        renderer.thinking(f"Generating {name}…")
                elif etype == "tool_call":
                    _flush_dedup()
                    # Hide thinking before collecting tool calls — the LLM
                    # has decided on an action, no longer "thinking".
                    if hasattr(renderer, "thinking_hide"):
                        renderer.thinking_hide()
                    result.tool_calls.append(event)
                elif etype == "session_info":
                    result.session_id = event.get("session_id")
                    result.run_id = event.get("run_id")
                elif etype == "usage":
                    result.usage = {k: v for k, v in event.items() if k != "type"}
                elif etype == "turn_complete":
                    result.has_tool_calls = event.get("has_tool_calls", False)
                elif etype == "error":
                    result.error = event
                    renderer.error(event.get("message", "Unknown cloud error"))
                elif etype == "ping":
                    # Heartbeat — connection alive. Show thinking indicator
                    # so the user knows the LLM is still working.
                    if hasattr(renderer, "thinking"):
                        renderer.thinking()
            # Stream ended — flush any remaining dedup buffer
            _flush_dedup()
    except TimeoutError:
        if deadline.expired():
            # Our wall-clock deadline fired — report as client timeout.
            result.error = {"type": "error", "message": "Turn timed out", "code": "CLIENT_TIMEOUT"}
            renderer.error("Turn timed out")
        else:
            # TimeoutError from the stream itself (e.g. network) — let caller handle.
            raise
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
    session_info: dict[str, Any] | None = None,
    extra_rules: str | None = None,
) -> str:
    """Run the edge-cloud agentic loop until final answer or MAX_TURNS.

    Returns the final assistant text.
    """
    renderer = renderer or StderrRenderer()
    messages: list[dict[str, Any]] = [{"role": "user", "content": user_input}]
    tool_results: list[dict[str, Any]] = []
    project_rules = load_project_rules(project_root)
    # extra_rules (e.g. skill dev context) are merged into project_rules here.
    # project_rules are only sent on turn 0 — the cloud caches them for the session.
    if extra_rules:
        project_rules = (project_rules + "\n\n" + extra_rules) if project_rules else extra_rules
    edge_profile = detect_edge_profile(project_root)
    if session_info is not None:
        session_info["has_project_rules"] = project_rules is not None
        session_info["has_edge_profile"] = bool(edge_profile)

    final_text = ""
    prev_turn_text = ""  # text from previous turn — used to suppress LLM repeats
    total_usage: dict[str, int] = {}

    try:
        for turn in range(MAX_TURNS):
            if session_info is not None:
                session_info["turn"] = turn

            # Always send edge_tools so the server has them even after
            # a restart (server-side session cache is in-memory only).
            current_schemas = tool_router.get_schemas()
            send_edge_tools = current_schemas

            # Call cloud with retry for transient errors
            _MAX_RETRIES = 2
            _BACKOFF = [1.0, 3.0]
            result = TurnResult()
            for attempt in range(_MAX_RETRIES + 1):
                try:
                    sse_stream = api_client.chat_turn(
                        messages=messages,
                        session_id=session_id,
                        tool_results=tool_results if tool_results else None,
                        project_rules=project_rules if turn == 0 else None,
                        agent_id=agent_id,
                        model=model,
                        edge_tools=send_edge_tools,
                        edge_profile=edge_profile if turn == 0 else None,
                    )
                    result = await _consume_turn(sse_stream, renderer, suppress_prefix=prev_turn_text)
                    if result.error and result.error.get("retryable") and attempt < _MAX_RETRIES:
                        delay = result.error.get("retry_after_ms", _BACKOFF[attempt] * 1000) / 1000
                        renderer.info(f"  ⟳ Retrying in {delay:.0f}s...")
                        await asyncio.sleep(delay)
                        continue
                    break
                # httpx.TransportError covers ReadError, ConnectError,
                # TimeoutException, etc. — all transient network failures.
                # Standard ConnectionError/OSError catch non-httpx sources.
                except (ConnectionError, OSError, TimeoutError, httpx.TransportError) as e:
                    if isinstance(e, (httpx.UnsupportedProtocol, httpx.LocalProtocolError)):
                        # Config or client bug — not transient, don't retry.
                        renderer.error(f"{type(e).__name__}: {e}")
                        break
                    if attempt < _MAX_RETRIES:
                        renderer.info(f"  ⟳ Network error, retrying in {_BACKOFF[attempt]:.0f}s...")
                        await asyncio.sleep(_BACKOFF[attempt])
                        continue
                    renderer.error(f"Network error: {e}")
                    break
                except KeyboardInterrupt:
                    renderer.error("Interrupted by user")
                    return final_text
                except AuthenticationError:
                    raise  # propagate to CLI for re-login prompt
                except Exception as e:
                    renderer.error(f"{type(e).__name__}: {e}")
                    break

            # Track session from first response
            if result.session_id and not session_id:
                session_id = result.session_id

            final_text = result.text
            for k, v in result.usage.items():
                total_usage[k] = total_usage.get(k, 0) + (v if isinstance(v, int) else 0)

            if not result.has_tool_calls:
                break

            # Remember this turn's text so the next turn can suppress if LLM repeats it.
            prev_turn_text = result.text

            # Execute tool calls locally
            parsed = ToolRouter.parse_tool_calls(result.tool_calls)
            approved: list[ToolCall] = []
            tool_results = []

            for tc in parsed:
                tool = tool_router.get_tool(tc.name)
                if tool is None:
                    tool_results.append({"tool_call_id": tc.id, "name": tc.name, "result": f"Unknown tool: {tc.name}"})
                    continue

                side_effect = resolve_side_effect(tool)

                decision = permissions.check(tc.name, side_effect, tc.arguments)

                if decision == Decision.DENY:
                    renderer.tool_start(tc.name, tc.arguments)
                    renderer.tool_done(tc.name, "Blocked (dangerous)", True)
                    tool_results.append({"tool_call_id": tc.id, "name": tc.name, "result": "Permission denied: command blocked by safety policy"})
                    continue

                if decision == Decision.ASK:
                    # End streaming markdown before showing interactive prompt,
                    # otherwise the permission panel renders on top of raw text.
                    if hasattr(renderer, "end_response"):
                        renderer.end_response()
                    decision = permissions.prompt_user(tc.name, side_effect, tc.arguments)
                    if decision == Decision.DENY:
                        renderer.tool_done(tc.name, "Denied by user", True)
                        tool_results.append({"tool_call_id": tc.id, "name": tc.name, "result": "Permission denied by user"})
                        continue

                approved.append(tc)

            # Execute approved tools concurrently
            if approved:
                results = await tool_router.execute(approved)
                for tc, r in zip(approved, results):
                    renderer.tool_start(tc.name, tc.arguments)
                    renderer.tool_done(r.name, r.result, r.error)
                    tool_results.append({"tool_call_id": r.tool_call_id, "name": r.name, "result": r.result})

            # Clear messages after first turn — cloud has the history
            messages = []
        else:
            renderer.error(f"Reached maximum turns ({MAX_TURNS})")
    finally:
        if hasattr(renderer, "stats"):
            renderer.stats(total_usage)

    return final_text
