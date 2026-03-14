"""Search tools — grep and glob."""

import asyncio
import fnmatch
import re
from pathlib import Path
from typing import Any

from cli.tools.base import EdgeTool, SideEffect
from cli.tools._gitignore import load_gitignore

MAX_MATCHES = 200
MAX_OUTPUT = 30 * 1024  # 30KB (~7K tokens)


class GrepTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root
        self._ignore_spec = load_gitignore(project_root)

    name = "grep"
    description = "Search for a regex pattern in files. Use to find usages, definitions, or text across the codebase."
    parameters = {
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regex pattern to search for"},
            "path": {
                "type": "string",
                "description": "Directory to search (default: project root)",
            },
            "include": {"type": "string", "description": "File glob filter (e.g. '*.py', '*.go')"},
        },
        "required": ["pattern"],
    }
    side_effect = SideEffect.READ

    async def execute(
        self, pattern: str, path: str = ".", include: str | None = None, **_: Any
    ) -> str:
        # Use ripgrep if available, fall back to Python
        search_path = Path(self._root) / path if not Path(path).is_absolute() else Path(path)
        if not search_path.exists():
            return f"Error: Path not found: {path}"

        # Try ripgrep first (much faster)
        rg = await self._try_ripgrep(pattern, str(search_path), include)
        if rg is not None:
            return rg

        # Fallback: Python regex search
        return await asyncio.to_thread(self._python_grep, pattern, search_path, include)

    async def _try_ripgrep(self, pattern: str, path: str, include: str | None) -> str | None:
        args = ["rg", "--no-heading", "--line-number", "--max-count", "50", "-e", pattern]
        if include:
            args.extend(["--glob", include])
        args.append(path)
        try:
            proc = await asyncio.create_subprocess_exec(
                *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=30)
            if proc.returncode in (0, 1):  # 1 = no matches
                out = stdout.decode(errors="replace")
                if len(out) > MAX_OUTPUT:
                    out = out[:MAX_OUTPUT] + "\n... truncated"
                return out or "No matches found"
        except (FileNotFoundError, asyncio.TimeoutError):
            pass
        return None

    def _python_grep(self, pattern: str, search_path: Path, include: str | None) -> str:
        try:
            regex = re.compile(pattern, re.IGNORECASE)
        except re.error as e:
            return f"Error: Invalid regex: {e}"

        matches: list[str] = []
        for fp in self._iter_files(search_path, include):
            try:
                for i, line in enumerate(fp.read_text(errors="replace").splitlines(), 1):
                    if regex.search(line):
                        rel = fp.relative_to(Path(self._root))
                        matches.append(f"{rel}:{i}:{line.rstrip()}")
                        if len(matches) >= MAX_MATCHES:
                            matches.append(f"... truncated at {MAX_MATCHES} matches")
                            return "\n".join(matches)
            except (PermissionError, OSError):
                continue
        return "\n".join(matches) if matches else "No matches found"

    def _iter_files(self, root: Path, include: str | None) -> list[Path]:
        files = []
        for item in root.rglob("*"):
            if item.name.startswith(".") or any(
                p.startswith(".") for p in item.relative_to(root).parts
            ):
                continue
            if self._ignore_spec:
                try:
                    rel = str(item.relative_to(Path(self._root)))
                    if self._ignore_spec.match_file(rel + ("/" if item.is_dir() else "")):
                        continue
                except ValueError:
                    pass  # item outside project root (e.g. symlink); skip filter
            if item.is_file():
                if include and not fnmatch.fnmatch(item.name, include):
                    continue
                files.append(item)
        return files


class GlobTool(EdgeTool):
    def __init__(self, project_root: str):
        self._root = project_root
        self._ignore_spec = load_gitignore(project_root)

    name = "glob"
    description = "Find files by glob pattern. Use to locate files by name or extension."
    parameters = {
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern (e.g. '**/*.py', 'src/**/*.go')",
            },
            "path": {"type": "string", "description": "Base directory (default: project root)"},
        },
        "required": ["pattern"],
    }
    side_effect = SideEffect.READ

    async def execute(self, pattern: str, path: str = ".", **_: Any) -> str:
        base = Path(self._root) / path if not Path(path).is_absolute() else Path(path)
        if not base.is_dir():
            return f"Error: Not a directory: {path}"

        # .git is not in .gitignore (git never ignores itself), so hardcode it
        skip = {".git"}
        results: list[str] = []
        for match in sorted(base.glob(pattern)):
            if any(p in match.parts for p in skip):
                continue
            if self._ignore_spec:
                try:
                    rel_str = str(match.relative_to(Path(self._root)))
                    if self._ignore_spec.match_file(rel_str + ("/" if match.is_dir() else "")):
                        continue
                except ValueError:
                    pass
            rel = match.relative_to(Path(self._root))
            results.append(f"{rel}/" if match.is_dir() else str(rel))
            if len(results) >= MAX_MATCHES:
                results.append(f"... truncated at {MAX_MATCHES} entries")
                break
        return "\n".join(results) if results else "No matches found"


def register_search_tools(router: Any, project_root: str) -> None:
    """Register search tools with the router."""
    router.register(GrepTool(project_root))
    router.register(GlobTool(project_root))
