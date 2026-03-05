"""Tests for core/skills/loader.py — SkillManifest, load_manifests, SkillLoader."""

import pytest
from pathlib import Path

from core.skills.loader import (
    SkillManifest,
    SkillLoader,
    LocalSkill,
    load_manifests,
    _load_skill_py,
    SKILL_FILENAME,
    MANIFEST_FILENAME,
)


class TestSkillManifest:
    def test_defaults(self):
        m = SkillManifest(name="test", version="1.0")
        assert m.description == ""
        assert m.depends_on == []
        assert m.tables == []


class TestLoadManifests:
    def test_nonexistent_dir(self, tmp_path):
        result = load_manifests(tmp_path / "nonexistent")
        assert result == []

    def test_empty_dir(self, tmp_path):
        skills_dir = tmp_path / "skills"
        skills_dir.mkdir()
        result = load_manifests(skills_dir)
        assert result == []

    def test_load_valid_manifest(self, tmp_path):
        skill_dir = tmp_path / "my_skill"
        skill_dir.mkdir()
        (skill_dir / "manifest.yaml").write_text(
            "name: my_skill\nversion: '1.0.0'\ndescription: Test skill\n"
        )
        result = load_manifests(tmp_path)
        assert len(result) == 1
        assert result[0].name == "my_skill"
        assert result[0].version == "1.0.0"

    def test_skip_non_directory(self, tmp_path):
        (tmp_path / "not_a_dir.txt").write_text("hello")
        result = load_manifests(tmp_path)
        assert result == []

    def test_skip_dir_without_manifest(self, tmp_path):
        (tmp_path / "no_manifest").mkdir()
        result = load_manifests(tmp_path)
        assert result == []

    def test_invalid_manifest_no_name(self, tmp_path):
        skill_dir = tmp_path / "bad_skill"
        skill_dir.mkdir()
        (skill_dir / "manifest.yaml").write_text("version: '1.0'\n")
        result = load_manifests(tmp_path)
        assert result == []


class TestLoadSkillPy:
    def test_no_skill_py(self, tmp_path):
        assert _load_skill_py(tmp_path) is None

    def test_valid_skill_py(self, tmp_path):
        (tmp_path / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "class MySkill(Skill[SkillInput, SkillOutput]):\n"
            "    name = 'my_test_skill'\n"
            "    description = 'test'\n"
            "    async def execute(self, input): pass\n"
        )
        result = _load_skill_py(tmp_path)
        assert result is not None
        assert result.name == "my_test_skill"

    def test_broken_skill_py(self, tmp_path):
        (tmp_path / "skill.py").write_text("raise RuntimeError('broken')\n")
        result = _load_skill_py(tmp_path)
        assert result is None

    def test_skill_py_no_subclass(self, tmp_path):
        (tmp_path / "skill.py").write_text("x = 42\n")
        result = _load_skill_py(tmp_path)
        assert result is None


class TestSkillLoader:
    def test_discover_empty(self, tmp_path):
        result = SkillLoader.discover([tmp_path])
        assert result == []

    def test_discover_skill_md(self, tmp_path):
        skill_dir = tmp_path / "greet"
        skill_dir.mkdir()
        (skill_dir / SKILL_FILENAME).write_text(
            "---\nname: greet\ndescription: Greet user\ntriggers:\n  - hello\n  - hi\n---\n"
            "# Greet\nSay hello.\n"
        )
        result = SkillLoader.discover([tmp_path])
        assert len(result) == 1
        assert result[0].skill.name == "greet"

    def test_discover_prefers_skill_py(self, tmp_path):
        skill_dir = tmp_path / "dual"
        skill_dir.mkdir()
        (skill_dir / SKILL_FILENAME).write_text(
            "---\nname: dual\ndescription: test\ntriggers:\n  - test\n---\ntest\n"
        )
        (skill_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "class DualSkill(Skill[SkillInput, SkillOutput]):\n"
            "    name = 'dual'\n"
            "    description = 'from py'\n"
            "    async def execute(self, input): pass\n"
        )
        result = SkillLoader.discover([tmp_path])
        assert len(result) == 1
        assert result[0].spec is None

    def test_discover_priority_order(self, tmp_path):
        dir1 = tmp_path / "high"
        dir2 = tmp_path / "low"
        for d in (dir1, dir2):
            sd = d / "conflict"
            sd.mkdir(parents=True)
            (sd / SKILL_FILENAME).write_text(
                f"---\nname: conflict\ndescription: from {d.name}\ntriggers:\n  - test\n---\ntest\n"
            )
        result = SkillLoader.discover([dir1, dir2])
        assert len(result) == 1
        assert result[0].skill.description == "from high"

    def test_default_paths(self, tmp_path):
        paths = SkillLoader.default_paths(tmp_path)
        assert len(paths) == 2
        assert ".mo-agent/skills" in str(paths[0])
