"""File operation tools — read, write, replace, list directory."""

import os
from pathlib import Path
from typing import Any

from cli.tools.base import EdgeTool, SideEffect
from cli.tools._gitignore import load_gitignore

# Safety limits
MAX_READ_SIZE = 512 * 1024  # 512KB
MAX_LIST_ENTRIES = 2000
MAX_LIST_DEPTH = 10


def _resolve_path(path: str, project_root: str) -> Path:
    """Resolve path relative to project root with traversal protection."""
    p = Path(path)
    if not p.is_absolute():
        p = Path(project_root) / p
    resolved = p.resolve()
    root = Path(project_root).resolve()
    if not str(resolved).startswith(str(root)):
        raise PermissionError(f"Path {path} is outside project root {project_root}")
    return resolved


class ReadFileTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root

    name = "read_file"
    description = "Read file contents. Use to inspect code, configs, or any text file."
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path (relative to project root or absolute)"},
            "start_line": {"type": "integer", "description": "Start line (1-based, optional)"},
            "end_line": {"type": "integer", "description": "End line (inclusive, optional)"},
        },
        "required": ["path"],
    }
    side_effect = SideEffect.READ

    async def execute(self, path: str, start_line: int | None = None, end_line: int | None = None, **_: Any) -> str:
        resolved = _resolve_path(path, self._root)
        if not resolved.exists():
            return f"Error: File not found: {path}"
        if not resolved.is_file():
            return f"Error: Not a file: {path}"
        if resolved.stat().st_size > MAX_READ_SIZE:
            return f"Error: File too large ({resolved.stat().st_size} bytes, max {MAX_READ_SIZE})"

        text = resolved.read_text(errors="replace")
        if start_line is not None or end_line is not None:
            lines = text.splitlines(keepends=True)
            s = (start_line or 1) - 1
            e = end_line or len(lines)
            text = "".join(lines[s:e])
        return text


class WriteFileTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root

    name = "write_file"
    description = "Create a new file. Use str_replace to edit existing files."
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path (must not already exist, use str_replace to edit existing files)"},
            "content": {"type": "string", "description": "File content"},
        },
        "required": ["path", "content"],
    }
    side_effect = SideEffect.WRITE

    async def execute(self, path: str, content: str, **_: Any) -> str:
        resolved = _resolve_path(path, self._root)
        if resolved.exists():
            return (
                f"Error: File already exists: {path}. "
                "Use str_replace to edit existing files instead of overwriting."
            )
        resolved.parent.mkdir(parents=True, exist_ok=True)
        resolved.write_text(content)
        return f"Wrote {len(content)} bytes to {path}"


class StrReplaceTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root

    name = "str_replace"
    description = (
        "Edit files by replacing an exact string. old_str must match exactly once. "
        "Use empty new_str to delete."
    )
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path"},
            "old_str": {"type": "string", "description": "Exact string to find (must be unique in file). Include surrounding lines for uniqueness."},
            "new_str": {"type": "string", "description": "Replacement string. Use empty string to delete the matched text."},
        },
        "required": ["path", "old_str", "new_str"],
    }
    side_effect = SideEffect.WRITE

    async def execute(self, path: str, old_str: str, new_str: str, **_: Any) -> str:
        resolved = _resolve_path(path, self._root)
        if not resolved.is_file():
            return f"Error: File not found: {path}"
        text = resolved.read_text(errors="replace")
        count = text.count(old_str)
        if count == 0:
            # Provide a helpful snippet of the file for the LLM to retry
            lines = text.splitlines()
            snippet = "\n".join(lines[:30]) if len(lines) > 30 else text
            return (
                f"Error: old_str not found in {path}. "
                f"Make sure it matches the file content exactly (including whitespace). "
                f"First 30 lines of file:\n{snippet}"
            )
        if count > 1:
            return f"Error: old_str found {count} times in {path}. Include more surrounding context to make it unique."
        resolved.write_text(text.replace(old_str, new_str, 1))
        return f"Replaced in {path}"


class ListDirTool(EdgeTool):
    """List directory with progressive disclosure: directories show child counts."""

    _MAX_CHILD_SCAN = 10000  # Cap rglob scan to avoid blocking on huge dirs

    def __init__(self, project_root: str):
        self._root = project_root
        self._ignore_spec = load_gitignore(project_root)

    name = "list_dir"
    description = "List directory contents. Use to explore project structure or find files."
    parameters = {
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Directory path (default: project root)"},
            "depth": {"type": "integer", "description": "Max recursion depth (default: 1)"},
            "include_ignored": {"type": "boolean", "description": "Include gitignored files (default: false)"},
        },
    }
    side_effect = SideEffect.READ

    def _is_ignored(self, rel: str, is_dir: bool) -> bool:
        """Check if path matches .gitignore."""
        if not self._ignore_spec:
            return False
        return self._ignore_spec.match_file(rel + ("/" if is_dir else ""))

    def _count_children(self, directory: Path, base: Path, include_ignored: bool) -> int:
        """Count non-ignored file children recursively.

        Caps scan at _MAX_CHILD_SCAN to avoid blocking on huge directories
        (e.g. node_modules with include_ignored=True). Returns a lower-bound
        estimate when the cap is hit.
        """
        count = 0
        scanned = 0
        try:
            for item in directory.rglob("*"):
                scanned += 1
                if scanned > self._MAX_CHILD_SCAN:
                    return count  # lower-bound estimate
                if item.name.startswith("."):
                    continue
                if not include_ignored:
                    rel = str(item.relative_to(base))
                    if self._is_ignored(rel, item.is_dir()):
                        continue
                if item.is_file():
                    count += 1
        except (PermissionError, OSError):
            pass
        return count

    async def execute(self, path: str = ".", depth: int = 1, include_ignored: bool = False, **_: Any) -> str:
        resolved = _resolve_path(path, self._root)
        if not resolved.is_dir():
            return f"Error: Not a directory: {path}"

        base = _resolve_path(".", self._root)
        entries: list[str] = []
        depth = min(depth, MAX_LIST_DEPTH)
        self._walk(resolved, resolved, base, 0, depth, include_ignored, entries)
        if len(entries) >= MAX_LIST_ENTRIES:
            entries.append(f"... truncated at {MAX_LIST_ENTRIES} entries")
        return "\n".join(entries)

    def _walk(
        self, base: Path, current: Path, project_base: Path,
        level: int, max_depth: int, include_ignored: bool, out: list[str],
    ) -> None:
        if len(out) >= MAX_LIST_ENTRIES:
            return
        try:
            items = sorted(current.iterdir(), key=lambda p: (not p.is_dir(), p.name))
        except PermissionError:
            out.append(f"{current.relative_to(base)}/ [permission denied]")
            return
        for item in items:
            if item.name.startswith("."):
                continue
            rel = item.relative_to(base)
            # .gitignore filtering
            if not include_ignored:
                # Both paths are .resolve()'d by _resolve_path, so comparison is safe
                proj_rel = str(item.relative_to(project_base)) if project_base != base else str(rel)
                if self._is_ignored(proj_rel, item.is_dir()):
                    continue
            if item.is_dir():
                if level < max_depth:
                    out.append(f"{rel}/")
                    self._walk(base, item, project_base, level + 1, max_depth, include_ignored, out)
                else:
                    # At depth limit: show count instead of expanding
                    count = self._count_children(item, project_base, include_ignored)
                    out.append(f"{rel}/  ({count} files)")
            else:
                out.append(str(rel))
            if len(out) >= MAX_LIST_ENTRIES:
                return


def register_file_tools(router: Any, project_root: str) -> None:
    """Register all file tools with the router."""
    router.register(ReadFileTool(project_root))
    router.register(WriteFileTool(project_root))
    router.register(StrReplaceTool(project_root))
    router.register(ListDirTool(project_root))
