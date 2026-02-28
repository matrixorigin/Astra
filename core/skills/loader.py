"""SkillLoader — discover and load SKILL.md files and manifest.yaml from skill directories."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from core.skills.markdown_skill import MarkdownSkill
from core.skills.skill_md import SkillMd, parse_skill_md

logger = logging.getLogger(__name__)

SKILL_DIR_NAME = ".mo-agent/skills"
SKILL_FILENAME = "SKILL.md"
MANIFEST_FILENAME = "manifest.yaml"


@dataclass
class SkillManifest:
    """Parsed manifest.yaml for a platform skill package."""
    name: str
    version: str
    description: str = ""
    table_prefix: str = ""
    tables: list[str] = field(default_factory=list)
    credentials: list[dict[str, Any]] = field(default_factory=list)
    requires: list[str] = field(default_factory=list)
    depends_on: list[str] = field(default_factory=list)
    author: str = ""
    path: Path = field(default_factory=lambda: Path("."))


def load_manifests(skills_root: Path | None = None) -> list[SkillManifest]:
    """Discover and parse all skills/*/manifest.yaml files.

    Args:
        skills_root: Root of skills directory. Defaults to <project>/skills/.
    """
    if skills_root is None:
        skills_root = Path(__file__).resolve().parent.parent.parent / "skills"
    if not skills_root.is_dir():
        return []

    try:
        import yaml
    except ImportError:
        logger.warning("PyYAML not installed; cannot load manifest.yaml files")
        return []

    manifests: list[SkillManifest] = []
    for skill_dir in sorted(skills_root.iterdir()):
        if not skill_dir.is_dir():
            continue
        manifest_path = skill_dir / MANIFEST_FILENAME
        if not manifest_path.is_file():
            continue
        try:
            data = yaml.safe_load(manifest_path.read_text())
            if not isinstance(data, dict) or "name" not in data:
                logger.warning("Invalid manifest: %s", manifest_path)
                continue
            manifests.append(SkillManifest(
                name=data["name"],
                version=data.get("version", "0.0.0"),
                description=data.get("description", ""),
                table_prefix=data.get("table_prefix", ""),
                tables=data.get("tables", []),
                credentials=data.get("credentials", []),
                requires=data.get("requires", []),
                depends_on=data.get("depends_on", []),
                author=data.get("author", ""),
                path=skill_dir,
            ))
            logger.info("Loaded manifest: %s@%s from %s", data["name"], data.get("version"), manifest_path)
        except Exception as e:
            logger.warning("Failed to parse %s: %s", manifest_path, e)

    return manifests


@dataclass
class LocalSkill:
    """A discovered local skill."""
    spec: SkillMd | None
    skill: Any  # MarkdownSkill or typed Skill from skill.py


def _load_skill_py(skill_dir: Path) -> Any | None:
    """Try to import a Skill subclass from skill_dir/skill.py."""
    py_path = skill_dir / "skill.py"
    if not py_path.is_file():
        return None
    import importlib.util
    try:
        spec = importlib.util.spec_from_file_location(f"_skill_{skill_dir.name}", py_path)
        if spec is None or spec.loader is None:
            return None
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        # Find the Skill subclass (not base classes)
        from core.skills.base import Skill as SkillBase
        for attr in vars(mod).values():
            if (isinstance(attr, type) and issubclass(attr, SkillBase)
                    and attr is not SkillBase and not attr.__name__.startswith("_")):
                return attr()
        return None
    except Exception as e:
        logger.warning("Failed to load skill.py from %s: %s", skill_dir, e)
        return None


class SkillLoader:
    """Discover SKILL.md files from multiple directories.

    Search order determines priority: earlier paths win on name conflicts.
    Typical usage: [project/.mo-agent/skills, ~/.mo-agent/skills]
    """

    @staticmethod
    def discover(paths: list[Path]) -> list[LocalSkill]:
        """Scan directories for skills. Prefers skill.py over SKILL.md."""
        seen: dict[str, LocalSkill] = {}

        for base in paths:
            if not base.is_dir():
                continue
            for skill_dir in sorted(base.iterdir()):
                if not skill_dir.is_dir():
                    continue

                # Try skill.py first
                typed_skill = _load_skill_py(skill_dir)
                if typed_skill is not None:
                    if typed_skill.name in seen:
                        continue
                    seen[typed_skill.name] = LocalSkill(spec=None, skill=typed_skill)
                    logger.info("Loaded typed skill: %s from %s/skill.py", typed_skill.name, skill_dir)
                    continue

                # Fall back to SKILL.md
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
