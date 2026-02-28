"""SkillLoader — discover and load SKILL.md files from local directories."""

from __future__ import annotations

import logging
from dataclasses import dataclass
from pathlib import Path

from core.skills.markdown_skill import MarkdownSkill
from core.skills.skill_md import SkillMd, parse_skill_md

logger = logging.getLogger(__name__)

SKILL_DIR_NAME = ".mo-agent/skills"
SKILL_FILENAME = "SKILL.md"


@dataclass
class LocalSkill:
    """A discovered local skill."""
    spec: SkillMd
    skill: MarkdownSkill


class SkillLoader:
    """Discover SKILL.md files from multiple directories.

    Search order determines priority: earlier paths win on name conflicts.
    Typical usage: [project/.mo-agent/skills, ~/.mo-agent/skills]
    """

    @staticmethod
    def discover(paths: list[Path]) -> list[LocalSkill]:
        """Scan directories for SKILL.md files. Earlier paths have higher priority."""
        seen: dict[str, LocalSkill] = {}

        for base in paths:
            if not base.is_dir():
                continue
            for skill_dir in sorted(base.iterdir()):
                if not skill_dir.is_dir():
                    continue
                md_path = skill_dir / SKILL_FILENAME
                if not md_path.is_file():
                    continue

                spec = parse_skill_md(md_path)
                if spec is None:
                    continue

                if spec.name in seen:
                    logger.debug(
                        "Skipping %s from %s (already loaded from higher-priority path)",
                        spec.name, md_path,
                    )
                    continue

                seen[spec.name] = LocalSkill(spec=spec, skill=MarkdownSkill(spec))
                logger.info("Loaded local skill: %s from %s", spec.name, md_path)

        return list(seen.values())

    @staticmethod
    def default_paths(project_root: str | Path) -> list[Path]:
        """Return default search paths: [project-level, user-level]."""
        return [
            Path(project_root) / SKILL_DIR_NAME,
            Path.home() / SKILL_DIR_NAME,
        ]
