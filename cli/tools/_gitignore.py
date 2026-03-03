"""Shared .gitignore loading for CLI tools."""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pathspec


def load_gitignore(project_root: str) -> "pathspec.PathSpec | None":
    """Load .gitignore patterns from project root.

    Returns None if .gitignore doesn't exist or pathspec is unavailable.
    """
    try:
        import pathspec
        gi = Path(project_root) / ".gitignore"
        if gi.exists():
            return pathspec.PathSpec.from_lines("gitignore", gi.read_text().splitlines())
    except Exception:
        pass
    return None
