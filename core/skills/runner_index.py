"""Lightweight skill-name index for the standalone runner.

This module intentionally avoids importing heavyweight runtime, DB, or skill
implementation modules so the runner can reject obvious misses quickly.
"""

from __future__ import annotations

from functools import lru_cache
from pathlib import Path
import re

_PYTHON_SKILL_NAME_RE = re.compile(r'^\s*name\s*=\s*"([^"]+)"', re.MULTILINE)
_MARKDOWN_SKILL_NAME_RE = re.compile(r"^name:\s*(.+?)\s*$", re.MULTILINE)

# Skills registered directly by core.skills.builtin.register_builtin_skills()
# when the standalone runner initializes its registry.
_RUNNER_BUILTIN_SKILL_NAMES = frozenset(
    {
        "summarize_pr",
        "list_prs",
        "ci_status",
        "list_issues",
        "get_issue",
        "create_issue",
        "execute_code",
        "introspection",
        "skill_config_wizard",
    }
)


def _project_root() -> Path:
    return Path(__file__).resolve().parents[2]


def _extract_python_skill_name(path: Path) -> str | None:
    try:
        match = _PYTHON_SKILL_NAME_RE.search(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError):
        return None
    return match.group(1) if match else None


def _extract_markdown_skill_name(path: Path) -> str | None:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return None

    stripped = text.lstrip()
    if not stripped.startswith("---"):
        return None

    _, _, remainder = stripped.partition("---")
    frontmatter, separator, _ = remainder.partition("\n---")
    if not separator:
        return None

    match = _MARKDOWN_SKILL_NAME_RE.search(frontmatter)
    if not match:
        return None

    return match.group(1).strip().strip("\"'")


@lru_cache(maxsize=1)
def discover_runner_skill_names() -> frozenset[str]:
    """Return skill names that the runner can recognize without full startup."""
    root = _project_root()
    names = set(_RUNNER_BUILTIN_SKILL_NAMES)

    for py_path in (root / "skills").rglob("*.py"):
        name = _extract_python_skill_name(py_path)
        if name:
            names.add(name)

    for skills_root in (root / ".mo-agent" / "skills", Path.home() / ".mo-agent" / "skills"):
        if not skills_root.is_dir():
            continue
        for py_path in skills_root.rglob("skill.py"):
            name = _extract_python_skill_name(py_path)
            if name:
                names.add(name)
        for md_path in skills_root.rglob("SKILL.md"):
            name = _extract_markdown_skill_name(md_path)
            if name:
                names.add(name)

    return frozenset(names)


def runner_skill_exists(skill_name: str) -> bool:
    """Cheap preflight check for standalone runner skill lookup."""
    return skill_name in discover_runner_skill_names()
