"""Git operation tools — status, diff, log."""

import asyncio
from typing import Any

from cli.tools.base import EdgeTool, SideEffect

MAX_OUTPUT = 30 * 1024  # 30KB (~7K tokens)


async def _find_git_root(project_root: str) -> str | None:
    """Find the git repo root from project_root (handles subdirectories)."""
    proc = await asyncio.create_subprocess_exec(
        "git", "rev-parse", "--show-toplevel",
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        cwd=project_root,
    )
    stdout, _ = await proc.communicate()
    return stdout.decode().strip() if proc.returncode == 0 else None


async def _git(project_root: str, *args: str) -> str:
    """Run a git command and return output. Auto-detects git root."""
    git_root = await _find_git_root(project_root)
    cwd = git_root or project_root
    proc = await asyncio.create_subprocess_exec(
        "git", *args,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        cwd=cwd,
    )
    stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30)
    if proc.returncode != 0:
        err = stderr.decode(errors="replace").strip()
        if "not a git repository" in err.lower():
            return "Error: Not a git repository"
        return f"Error: {err}"
    out = stdout.decode(errors="replace")
    if len(out) > MAX_OUTPUT:
        out = out[:MAX_OUTPUT] + f"\n... truncated"
    return out or "(no output)"


class GitStatusTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root

    name = "git_status"
    description = "Show git working tree status."
    parameters = {"type": "object", "properties": {}}
    side_effect = SideEffect.READ

    async def execute(self, **_: Any) -> str:
        return await _git(self._root, "status", "--porcelain=v1")


class GitDiffTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root

    name = "git_diff"
    description = "Show git diff. Use staged=true to see changes staged for commit."
    parameters = {
        "type": "object",
        "properties": {
            "ref": {"type": "string", "description": "Git ref to diff against (e.g. HEAD~1, main)"},
            "staged": {"type": "boolean", "description": "If true, show staged (cached) changes instead of unstaged"},
        },
    }
    side_effect = SideEffect.READ

    async def execute(self, ref: str | None = None, staged: bool = False, **_: Any) -> str:
        args = ["diff"]
        if staged:
            args.append("--cached")
        if ref:
            args.append(ref)
        return await _git(self._root, *args)


class GitLogTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root

    name = "git_log"
    description = "Show recent git commits."
    parameters = {
        "type": "object",
        "properties": {
            "n": {"type": "integer", "description": "Number of commits (default: 10)"},
        },
    }
    side_effect = SideEffect.READ

    async def execute(self, n: int = 10, **_: Any) -> str:
        return await _git(self._root, "log", f"-{n}", "--oneline", "--no-decorate")


def register_git_tools(router: Any, project_root: str) -> None:
    """Register git tools with the router."""
    router.register(GitStatusTool(project_root))
    router.register(GitDiffTool(project_root))
    router.register(GitLogTool(project_root))
