"""Tests for SkillLoader — discover SKILL.md files from local directories."""

from pathlib import Path

from core.skills.loader import SKILL_DIR_NAME, SKILL_FILENAME, SkillLoader


def _write_skill_md(base: Path, name: str, *, version: str = "1.0.0", desc: str = "A skill") -> Path:
    """Create a SKILL.md file under base/name/SKILL.md."""
    d = base / name
    d.mkdir(parents=True, exist_ok=True)
    md = d / SKILL_FILENAME
    md.write_text(
        f"---\nname: {name}\ndescription: {desc}\nversion: {version}\n---\nDo the thing.\n",
        encoding="utf-8",
    )
    return md


class TestDiscover:
    def test_finds_skill(self, tmp_path):
        _write_skill_md(tmp_path, "greet")
        results = SkillLoader.discover([tmp_path])
        assert len(results) == 1
        assert results[0].spec.name == "greet"
        assert results[0].skill.name == "greet"

    def test_skips_missing_dir(self, tmp_path):
        assert SkillLoader.discover([tmp_path / "nope"]) == []

    def test_skips_malformed(self, tmp_path):
        d = tmp_path / "bad"
        d.mkdir()
        (d / SKILL_FILENAME).write_text("no frontmatter here", encoding="utf-8")
        assert SkillLoader.discover([tmp_path]) == []

    def test_earlier_path_wins(self, tmp_path):
        hi = tmp_path / "hi"
        lo = tmp_path / "lo"
        _write_skill_md(hi, "dup", desc="high priority")
        _write_skill_md(lo, "dup", desc="low priority")
        results = SkillLoader.discover([hi, lo])
        assert len(results) == 1
        assert results[0].spec.description == "high priority"

    def test_multiple_skills(self, tmp_path):
        _write_skill_md(tmp_path, "alpha")
        _write_skill_md(tmp_path, "beta")
        results = SkillLoader.discover([tmp_path])
        names = {r.spec.name for r in results}
        assert names == {"alpha", "beta"}

    def test_ignores_files_in_root(self, tmp_path):
        """Only subdirectories are scanned, not loose files."""
        (tmp_path / SKILL_FILENAME).write_text(
            "---\nname: loose\ndescription: x\n---\nbody\n", encoding="utf-8",
        )
        assert SkillLoader.discover([tmp_path]) == []

    def test_skips_dir_without_skill_md(self, tmp_path):
        (tmp_path / "empty_dir").mkdir()
        assert SkillLoader.discover([tmp_path]) == []


class TestDefaultPaths:
    def test_returns_two_paths(self, tmp_path):
        paths = SkillLoader.default_paths(tmp_path)
        assert len(paths) == 2
        assert paths[0] == tmp_path / SKILL_DIR_NAME
        assert paths[1] == Path.home() / SKILL_DIR_NAME
