"""SKILL.md parser — extract YAML frontmatter + markdown body."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from pathlib import Path

import yaml

logger = logging.getLogger(__name__)


@dataclass
class SkillMd:
    """Parsed SKILL.md content."""

    name: str
    description: str
    body: str  # markdown instructions
    version: str = "1.0.0"
    triggers: list[str] = field(default_factory=list)
    dependencies: list[str] = field(default_factory=list)
    llm_required: bool = True
    category: str = "user"
    priority: int = 5
    path: Path | None = None


def parse_skill_md(path: Path) -> SkillMd | None:
    """Parse a SKILL.md file into a SkillMd dataclass.

    Returns None if the file is missing required fields or malformed.
    """
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as e:
        logger.warning("Cannot read %s: %s", path, e)
        return None

    # Split YAML frontmatter from markdown body
    frontmatter, body = _split_frontmatter(text)
    if frontmatter is None:
        logger.warning("No YAML frontmatter in %s", path)
        return None

    try:
        meta = yaml.safe_load(frontmatter)
    except yaml.YAMLError as e:
        logger.warning("Invalid YAML in %s: %s", path, e)
        return None

    if not isinstance(meta, dict):
        logger.warning("Frontmatter is not a dict in %s", path)
        return None

    name = meta.get("name")
    description = meta.get("description")
    if not name or not description:
        logger.warning("Missing name or description in %s", path)
        return None

    return SkillMd(
        name=str(name),
        description=str(description),
        body=body.strip(),
        version=str(meta.get("version", "1.0.0")),
        triggers=meta.get("triggers") or [],
        dependencies=meta.get("dependencies") or [],
        llm_required=bool(meta.get("llm_required", True)),
        category=str(meta.get("category", "user")),
        priority=int(meta.get("priority", 5)),
        path=path,
    )


def _split_frontmatter(text: str) -> tuple[str | None, str]:
    """Split '---\\n...\\n---\\n...' into (frontmatter, body).

    The closing ``---`` may have trailing whitespace (e.g. ``---  \\n``).
    Body content may itself contain ``---`` (e.g. markdown horizontal rules);
    only the *first* line that is exactly ``---`` (after stripping trailing
    whitespace) closes the frontmatter.
    """
    stripped = text.lstrip()
    if not stripped.startswith("---"):
        return None, text

    # Skip the opening --- and any immediately following newline
    rest = stripped[3:].lstrip("\n")

    # Walk line-by-line to find the closing --- (tolerating trailing spaces)
    lines = rest.split("\n")
    for i, line in enumerate(lines):
        if line.rstrip() == "---":
            fm = "\n".join(lines[:i])
            body = "\n".join(lines[i + 1:])
            return fm, body

    # No closing --- found
    return None, text
