"""Shell execution tool — run commands on user's machine."""

import asyncio
import os
import signal
from typing import Any

from cli.tools.base import EdgeTool, SideEffect

DEFAULT_TIMEOUT = 120  # seconds
MAX_OUTPUT = 30 * 1024  # 30KB (~7K tokens)


class BashTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root

    name = "bash"
    description = "Execute a shell command. Working directory is the project root."
    parameters = {
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "Shell command to execute"},
            "timeout": {"type": "integer", "description": f"Timeout in seconds (default: {DEFAULT_TIMEOUT})"},
        },
        "required": ["command"],
    }
    side_effect = SideEffect.EXECUTE

    async def execute(self, command: str, timeout: float | int | None = None, **_: Any) -> str:
        timeout = timeout or DEFAULT_TIMEOUT
        proc = await asyncio.create_subprocess_shell(
            command,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=self._root,
            preexec_fn=os.setsid,  # new process group so we can kill the tree
        )
        try:
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        except asyncio.TimeoutError:
            # Kill entire process group, then reap with a short deadline
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                proc.kill()
            try:
                await asyncio.wait_for(proc.wait(), timeout=2)
            except asyncio.TimeoutError:
                pass
            return f"Error: Command timed out after {timeout}s"

        out = stdout.decode(errors="replace")
        err = stderr.decode(errors="replace")
        result = ""
        if out:
            result += out
        if err:
            result += f"\nSTDERR:\n{err}" if result else err
        if not result:
            result = "(no output)"

        # Truncate
        if len(result) > MAX_OUTPUT:
            result = result[:MAX_OUTPUT] + f"\n... truncated ({len(result)} bytes total)"

        if proc.returncode != 0:
            result += f"\n[exit code: {proc.returncode}]"
        return result


def register_shell_tools(router: Any, project_root: str) -> None:
    """Register shell tools with the router."""
    router.register(BashTool(project_root))
