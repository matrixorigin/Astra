"""Tests for /skill slash commands — new, test, dev, list."""

from io import StringIO

import pytest
from rich.console import Console

from cli.mo_agent_api import (
    _build_skill_dev_context,
    _normalize_skill_name,
    _to_class,
    _to_slug,
    _validate_skill_output,
    cmd_skill,
)


def _console():
    buf = StringIO()
    return Console(file=buf, force_terminal=False, width=120), buf


# Reusable echo skill source — typed Skill (not MarkdownSkill)
_ECHO_SKILL_PY = (
    'from core.skills.base import Skill, SkillInput, SkillOutput\n'
    'from pydantic import Field\n'
    'class EI(SkillInput):\n    query: str = ""\n'
    'class EO(SkillOutput):\n    echo: str = ""\n'
    'class EchoSkill(Skill[EI, EO]):\n'
    '    name = "echo"\n    version = "1.0.0"\n    description = "echo"\n'
    '    async def execute(self, input: EI) -> EO:\n'
    '        return EO(success=True, echo=input.query)\n'
)


@pytest.fixture
def echo_skill(tmp_path, monkeypatch):
    """Create a typed echo skill and chdir into the project."""
    monkeypatch.chdir(tmp_path)
    skill_dir = tmp_path / ".mo-agent" / "skills" / "echo"
    skill_dir.mkdir(parents=True)
    (skill_dir / "skill.py").write_text(_ECHO_SKILL_PY)
    return tmp_path


# ============================================================================
# /skill new
# ============================================================================

class TestSkillNew:
    def test_creates_loadable_skill(self, tmp_path, monkeypatch):
        """Generated skill.py can be loaded by SkillLoader."""
        monkeypatch.chdir(tmp_path)
        (tmp_path / ".mo-agent" / "skills").mkdir(parents=True)
        console, _ = _console()
        cmd_skill(console, cmd_arg="new my_tool")

        skill_dir = tmp_path / ".mo-agent" / "skills" / "my-tool"
        assert (skill_dir / "skill.py").exists()
        assert (skill_dir / "SKILL.md").exists()

        content = (skill_dir / "skill.py").read_text()
        assert "class MyToolSkill" in content
        assert "class MyToolInput" in content
        assert 'name = "my_tool"' in content

        from core.skills.loader import SkillLoader
        skills = SkillLoader.discover([tmp_path / ".mo-agent" / "skills"])
        assert len(skills) == 1
        assert skills[0].skill.name == "my_tool"

    def test_kebab_case_input_normalized(self, tmp_path, monkeypatch):
        """Kebab-case input produces valid Python class names."""
        monkeypatch.chdir(tmp_path)
        (tmp_path / ".mo-agent" / "skills").mkdir(parents=True)
        console, _ = _console()
        cmd_skill(console, cmd_arg="new my-tool")
        content = (tmp_path / ".mo-agent" / "skills" / "my-tool" / "skill.py").read_text()
        assert "class MyToolSkill" in content
        assert 'name = "my_tool"' in content

    def test_refuses_existing(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        (tmp_path / ".mo-agent" / "skills" / "dup").mkdir(parents=True)
        console, buf = _console()
        cmd_skill(console, cmd_arg="new dup")
        assert "already exists" in buf.getvalue()

    def test_no_name_shows_usage(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="new")
        assert "Usage" in buf.getvalue()


# ============================================================================
# /skill test
# ============================================================================

class TestSkillTest:
    def test_runs_typed_skill(self, echo_skill):
        console, buf = _console()
        cmd_skill(console, cmd_arg="test echo hello")
        output = buf.getvalue()
        assert "OUTPUT" in output
        assert "hello" in output

    def test_json_args(self, echo_skill):
        console, buf = _console()
        cmd_skill(console, cmd_arg='test echo {"query": "json_test"}')
        assert "json_test" in buf.getvalue()

    def test_not_found(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="test nonexistent")
        assert "not found" in buf.getvalue()

    def test_no_name_shows_usage(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="test")
        assert "Usage" in buf.getvalue()

    def test_raising_skill_shows_exception(self, tmp_path, monkeypatch):
        """Skill that raises shows EXCEPTION with the error message."""
        monkeypatch.chdir(tmp_path)
        skill_dir = tmp_path / ".mo-agent" / "skills" / "fail"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text(
            'from core.skills.base import Skill, SkillInput, SkillOutput\n'
            'class I(SkillInput):\n    query: str = ""\n'
            'class O(SkillOutput):\n    pass\n'
            'class FailSkill(Skill[I, O]):\n'
            '    name = "fail"\n    version = "1.0.0"\n    description = "fail"\n'
            '    async def execute(self, input):\n'
            '        raise ValueError("boom")\n'
        )
        console, buf = _console()
        cmd_skill(console, cmd_arg="test fail")
        output = buf.getvalue()
        # ToolRouter catches the exception and returns it as an error result
        assert "ERROR" in output or "EXCEPTION" in output
        assert "boom" in output

    def test_empty_output_shows_warning(self, tmp_path, monkeypatch):
        """Skill returning all-empty fields triggers validation warning."""
        monkeypatch.chdir(tmp_path)
        skill_dir = tmp_path / ".mo-agent" / "skills" / "empty"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text(
            'from core.skills.base import Skill, SkillInput, SkillOutput\n'
            'class I(SkillInput):\n    query: str = ""\n'
            'class O(SkillOutput):\n    data: dict = {}\n'
            'class EmptySkill(Skill[I, O]):\n'
            '    name = "empty"\n'
            '    version = "1.0.0"\n'
            '    description = "returns empty"\n'
            '    async def execute(self, input):\n'
            '        return O(success=True)\n'
        )
        console, buf = _console()
        cmd_skill(console, cmd_arg="test empty")
        output = buf.getvalue()
        assert "OUTPUT" in output
        # _validate_skill_output detects all-empty custom fields
        assert "empty" in output.lower()


# ============================================================================
# /skill dev
# ============================================================================

class TestSkillDev:
    def test_sets_state_with_file_contents(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        skill_dir = tmp_path / ".mo-agent" / "skills" / "my-tool"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text("# my implementation")
        console, buf = _console()
        state = {}
        cmd_skill(console, cmd_arg="dev my-tool", state=state)
        assert state["skill_dev_name"] == "my-tool"
        assert state["skill_dev_dir"] == str(skill_dir)
        assert "# my implementation" in state["skill_dev_context"]
        assert "SKILL DEV MODE" in state["skill_dev_context"]
        assert "Entered dev mode" in buf.getvalue()

    def test_normalizes_snake_case_input(self, tmp_path, monkeypatch):
        """'/skill dev my_tool' finds .mo-agent/skills/my-tool/ directory."""
        monkeypatch.chdir(tmp_path)
        skill_dir = tmp_path / ".mo-agent" / "skills" / "my-tool"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text("# code")
        console, buf = _console()
        state = {}
        cmd_skill(console, cmd_arg="dev my_tool", state=state)
        assert state["skill_dev_name"] == "my-tool"
        assert "Entered dev mode" in buf.getvalue()

    def test_dev_off_clears_all_state_keys(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        state = {"skill_dev_context": "ctx", "skill_dev_name": "x", "skill_dev_dir": "/x"}
        console, buf = _console()
        cmd_skill(console, cmd_arg="dev off", state=state)
        assert "skill_dev_context" not in state
        assert "skill_dev_name" not in state
        assert "skill_dev_dir" not in state
        assert "Exited" in buf.getvalue()

    def test_not_found(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="dev nonexistent", state={})
        assert "not found" in buf.getvalue()

    def test_no_name_shows_usage(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="dev", state={})
        assert "Usage" in buf.getvalue()

    def test_state_none_still_prints_success(self, tmp_path, monkeypatch):
        """When state=None (called outside REPL), still prints feedback."""
        monkeypatch.chdir(tmp_path)
        skill_dir = tmp_path / ".mo-agent" / "skills" / "my-tool"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text("# code")
        console, buf = _console()
        cmd_skill(console, cmd_arg="dev my-tool", state=None)
        assert "Entered dev mode" in buf.getvalue()


# ============================================================================
# /skill list
# ============================================================================

class TestSkillList:
    def test_shows_both_types(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        md_dir = tmp_path / ".mo-agent" / "skills" / "md-skill"
        md_dir.mkdir(parents=True)
        (md_dir / "SKILL.md").write_text(
            "---\nname: md_skill\nversion: 1.0.0\n"
            "description: markdown skill\n---\nbody"
        )
        py_dir = tmp_path / ".mo-agent" / "skills" / "py-skill"
        py_dir.mkdir(parents=True)
        (py_dir / "skill.py").write_text(
            'from core.skills.base import Skill, SkillInput, SkillOutput\n'
            'class I(SkillInput):\n    pass\n'
            'class O(SkillOutput):\n    pass\n'
            'class PySkill(Skill[I, O]):\n'
            '    name = "py_skill"\n    version = "1.0.0"\n    description = "python skill"\n'
            '    async def execute(self, input): return O(success=True)\n'
        )
        console, buf = _console()
        cmd_skill(console, cmd_arg="list")
        output = buf.getvalue()
        assert "md_skill" in output
        assert "py_skill" in output
        # Verify Type column distinguishes the two
        assert " md " in output or "md " in output
        assert " py " in output or "py " in output

    def test_empty(self, tmp_path, monkeypatch):
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console)
        assert "No local skills" in buf.getvalue()


# ============================================================================
# Helpers
# ============================================================================

class TestHelpers:
    def test_to_class_snake(self):
        assert _to_class("stock_basic_info") == "StockBasicInfo"

    def test_to_class_kebab(self):
        assert _to_class("my-tool") == "MyTool"

    def test_to_class_single(self):
        assert _to_class("echo") == "Echo"

    def test_normalize_skill_name(self):
        assert _normalize_skill_name("my-tool") == "my_tool"
        assert _normalize_skill_name("already_snake") == "already_snake"
        assert _normalize_skill_name("my-cool-tool") == "my_cool_tool"

    def test_to_slug(self):
        assert _to_slug("my_tool") == "my-tool"
        assert _to_slug("my-tool") == "my-tool"
        assert _to_slug("echo") == "echo"

    def test_validate_all_empty(self):
        w = _validate_skill_output({"success": True, "data": {}, "items": []})
        assert any("empty" in x for x in w)

    def test_validate_no_error_msg(self):
        w = _validate_skill_output({"success": False})
        assert any("error" in x for x in w)

    def test_validate_ok(self):
        assert _validate_skill_output({"success": True, "data": {"key": "val"}}) == []

    def test_build_skill_dev_context(self, tmp_path):
        skill_dir = tmp_path / "test-skill"
        skill_dir.mkdir()
        (skill_dir / "skill.py").write_text("# code")
        (skill_dir / "SKILL.md").write_text("# docs")
        ctx = _build_skill_dev_context("test_skill", skill_dir)
        assert "SKILL DEV MODE: test_skill" in ctx
        assert "# code" in ctx
        assert "# docs" in ctx
        assert "Framework" in ctx

    def test_build_skill_dev_context_skips_unreadable(self, tmp_path):
        """Unreadable files don't crash _build_skill_dev_context."""
        skill_dir = tmp_path / "broken-skill"
        skill_dir.mkdir()
        f = skill_dir / "skill.py"
        f.write_text("# code")
        f.chmod(0o000)
        try:
            ctx = _build_skill_dev_context("broken", skill_dir)
            assert "unreadable" in ctx
        finally:
            f.chmod(0o644)  # restore for cleanup
