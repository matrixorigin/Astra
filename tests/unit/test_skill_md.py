"""Tests for SKILL.md parser (skill_md.py)."""

from pathlib import Path

from core.skills.skill_md import SkillMd, parse_skill_md


def _write(tmp_path: Path, content: str, *, encoding: str = "utf-8") -> Path:
    p = tmp_path / "SKILL.md"
    p.write_text(content, encoding=encoding)
    return p


def _write_bytes(tmp_path: Path, data: bytes) -> Path:
    p = tmp_path / "SKILL.md"
    p.write_bytes(data)
    return p


class TestParseSkillMd:
    def test_basic(self, tmp_path):
        p = _write(tmp_path, "---\nname: foo\ndescription: bar\n---\nBody text.\n")
        spec = parse_skill_md(p)
        assert spec is not None
        assert spec.name == "foo"
        assert spec.description == "bar"
        assert spec.body == "Body text."
        assert spec.version == "1.0.0"
        assert spec.path == p

    def test_all_fields(self, tmp_path):
        content = (
            "---\n"
            "name: deploy\n"
            "description: Deploy service\n"
            "version: 2.3.0\n"
            "triggers:\n  - on_push\n"
            "dependencies:\n  - docker\n"
            "llm_required: false\n"
            "category: devops\n"
            "priority: 3\n"
            "---\nRun deploy steps.\n"
        )
        spec = parse_skill_md(_write(tmp_path, content))
        assert spec.version == "2.3.0"
        assert spec.triggers == ["on_push"]
        assert spec.dependencies == ["docker"]
        assert spec.llm_required is False
        assert spec.category == "devops"
        assert spec.priority == 3

    def test_no_frontmatter_returns_none(self, tmp_path):
        assert parse_skill_md(_write(tmp_path, "Just markdown, no frontmatter.")) is None

    def test_missing_name_returns_none(self, tmp_path):
        assert parse_skill_md(_write(tmp_path, "---\ndescription: x\n---\nbody\n")) is None

    def test_missing_description_returns_none(self, tmp_path):
        assert parse_skill_md(_write(tmp_path, "---\nname: x\n---\nbody\n")) is None

    def test_invalid_yaml_returns_none(self, tmp_path):
        assert parse_skill_md(_write(tmp_path, "---\n: bad: yaml: [[\n---\nbody\n")) is None

    def test_nonexistent_file_returns_none(self, tmp_path):
        assert parse_skill_md(tmp_path / "nope.md") is None

    def test_empty_body(self, tmp_path):
        spec = parse_skill_md(_write(tmp_path, "---\nname: x\ndescription: y\n---\n"))
        assert spec is not None
        assert spec.body == ""

    def test_frontmatter_not_dict_returns_none(self, tmp_path):
        assert parse_skill_md(_write(tmp_path, "---\n- list item\n---\nbody\n")) is None

    def test_unclosed_frontmatter_returns_none(self, tmp_path):
        assert parse_skill_md(_write(tmp_path, "---\nname: x\ndescription: y\n")) is None

    def test_multiline_body_with_headers_and_code(self, tmp_path):
        """Complex markdown body with headers, code blocks, and lists."""
        content = (
            "---\nname: complex\ndescription: complex skill\n---\n"
            "# Step 1\n\n"
            "Do this:\n\n"
            "```python\ndef hello():\n    print('hi')\n```\n\n"
            "- item a\n"
            "- item b\n\n"
            "## Step 2\n\n"
            "Done.\n"
        )
        spec = parse_skill_md(_write(tmp_path, content))
        assert spec is not None
        assert "# Step 1" in spec.body
        assert "```python" in spec.body
        assert "## Step 2" in spec.body
        assert "- item a" in spec.body

    def test_horizontal_rule_in_body(self, tmp_path):
        """Body containing --- (horizontal rule) must not be confused with frontmatter."""
        content = (
            "---\nname: hr_test\ndescription: has hr\n---\nBefore rule.\n\n---\n\nAfter rule.\n"
        )
        spec = parse_skill_md(_write(tmp_path, content))
        assert spec is not None
        assert "Before rule." in spec.body
        assert "After rule." in spec.body

    def test_closing_frontmatter_with_trailing_spaces(self, tmp_path):
        """Closing --- with trailing whitespace should still be recognized."""
        content = "---\nname: sp\ndescription: spaces\n---   \nBody.\n"
        spec = parse_skill_md(_write(tmp_path, content))
        assert spec is not None
        assert spec.name == "sp"
        assert spec.body == "Body."

    def test_non_utf8_file_returns_none(self, tmp_path):
        """Non-UTF-8 encoded file should return None, not crash."""
        # Latin-1 encoded bytes that are invalid UTF-8
        data = b"---\nname: bad\ndescription: enc\n---\nBody with \xe9\xe8\n"
        p = _write_bytes(tmp_path, data)
        assert parse_skill_md(p) is None
