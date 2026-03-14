"""Tests for /skill slash commands — new, test, dev, list."""

from io import StringIO

import pytest
from rich.console import Console

from cli.mo_agent_api import (
    _build_skill_dev_context,
    _generate_skill_template,
    _normalize_skill_name,
    _to_class,
    _to_slug,
    _validate_skill_output,
    _validate_skill_source,
    cmd_skill,
)


def _console():
    buf = StringIO()
    return Console(file=buf, force_terminal=False, width=120), buf


# Reusable echo skill source — typed Skill (not MarkdownSkill)
_ECHO_SKILL_PY = (
    "from core.skills.base import Skill, SkillInput, SkillOutput\n"
    "from pydantic import Field\n"
    'class EI(SkillInput):\n    query: str = ""\n'
    'class EO(SkillOutput):\n    echo: str = ""\n'
    "class EchoSkill(Skill[EI, EO]):\n"
    '    name = "echo"\n    version = "1.0.0"\n    description = "echo"\n'
    "    async def execute(self, input: EI) -> EO:\n"
    "        return EO(success=True, echo=input.query)\n"
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
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            'class I(SkillInput):\n    query: str = ""\n'
            "class O(SkillOutput):\n    pass\n"
            "class FailSkill(Skill[I, O]):\n"
            '    name = "fail"\n    version = "1.0.0"\n    description = "fail"\n'
            "    async def execute(self, input):\n"
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
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            'class I(SkillInput):\n    query: str = ""\n'
            "class O(SkillOutput):\n    data: dict = {}\n"
            "class EmptySkill(Skill[I, O]):\n"
            '    name = "empty"\n'
            '    version = "1.0.0"\n'
            '    description = "returns empty"\n'
            "    async def execute(self, input):\n"
            "        return O(success=True)\n"
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
            "---\nname: md_skill\nversion: 1.0.0\ndescription: markdown skill\n---\nbody"
        )
        py_dir = tmp_path / ".mo-agent" / "skills" / "py-skill"
        py_dir.mkdir(parents=True)
        (py_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "class I(SkillInput):\n    pass\n"
            "class O(SkillOutput):\n    pass\n"
            "class PySkill(Skill[I, O]):\n"
            '    name = "py_skill"\n    version = "1.0.0"\n    description = "python skill"\n'
            "    async def execute(self, input): return O(success=True)\n"
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
        # Must include full paths for str_replace
        assert str(skill_dir / "skill.py") in ctx
        assert "full path" in ctx.lower()

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


# ============================================================================
# /skill validate
# ============================================================================


class TestSkillValidate:
    """Tests for /skill validate command and _validate_skill_source."""

    def test_valid_skill_no_issues(self, tmp_path, monkeypatch):
        """Well-formed skill passes validation."""
        monkeypatch.chdir(tmp_path)
        skill_dir = tmp_path / ".mo-agent" / "skills" / "good"
        skill_dir.mkdir(parents=True)
        # Use Field(description=...) for input, and default for output
        (skill_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "from pydantic import Field\n"
            'class GI(SkillInput):\n    query: str = Field(description="test query")\n'
            'class GO(SkillOutput):\n    data: str = ""\n'
            "class GoodSkill(Skill[GI, GO]):\n"
            '    name = "good"\n    version = "1.0.0"\n    description = "A good skill"\n'
            "    async def execute(self, input: GI) -> GO:\n"
            "        return GO(success=True, data=input.query)\n"
        )
        console, buf = _console()
        cmd_skill(console, cmd_arg="validate good")
        output = buf.getvalue()
        assert "no issues" in output or "✓" in output

    def test_sync_execute_error(self, tmp_path):
        """Non-async execute() is flagged as error."""
        skill_dir = tmp_path / "sync-skill"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "class I(SkillInput):\n    pass\n"
            "class O(SkillOutput):\n    pass\n"
            "class SyncSkill(Skill[I, O]):\n"
            '    name = "sync"\n    version = "1.0.0"\n    description = "sync"\n'
            "    def execute(self, input):\n"  # Missing async!
            "        return O(success=True)\n"
        )
        issues = _validate_skill_source(skill_dir)
        errors = [msg for level, msg in issues if level == "error"]
        assert any("async" in msg for msg in errors)

    def test_raise_in_execute_warning(self, tmp_path):
        """raise inside execute() triggers warning."""
        skill_dir = tmp_path / "raise-skill"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "class I(SkillInput):\n    pass\n"
            "class O(SkillOutput):\n    pass\n"
            "class RaiseSkill(Skill[I, O]):\n"
            '    name = "raise_test"\n    version = "1.0.0"\n    description = "test"\n'
            "    async def execute(self, input):\n"
            '        raise ValueError("boom")\n'
        )
        issues = _validate_skill_source(skill_dir)
        warnings = [msg for level, msg in issues if level == "warning"]
        assert any("raise" in msg.lower() for msg in warnings)

    def test_missing_field_description_warning(self, tmp_path):
        """Fields without Field(description=...) trigger warning."""
        skill_dir = tmp_path / "no-desc-skill"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "class I(SkillInput):\n    query: str\n"  # No Field(description=...)
            'class O(SkillOutput):\n    data: str = ""\n'
            "class NoDescSkill(Skill[I, O]):\n"
            '    name = "no_desc"\n    version = "1.0.0"\n    description = "test"\n'
            "    async def execute(self, input):\n"
            "        return O(success=True)\n"
        )
        issues = _validate_skill_source(skill_dir)
        warnings = [msg for level, msg in issues if level == "warning"]
        assert any("query" in msg for msg in warnings)

    def test_missing_output_default_warning(self, tmp_path):
        """Output fields without defaults trigger warning."""
        skill_dir = tmp_path / "no-default-skill"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "from pydantic import Field\n"
            'class I(SkillInput):\n    query: str = Field(description="q")\n'
            "class O(SkillOutput):\n    data: str\n"  # No default!
            "class NoDefaultSkill(Skill[I, O]):\n"
            '    name = "no_default"\n    version = "1.0.0"\n    description = "test"\n'
            "    async def execute(self, input):\n"
            '        return O(success=True, data="x")\n'
        )
        issues = _validate_skill_source(skill_dir)
        warnings = [msg for level, msg in issues if level == "warning"]
        assert any("data" in msg for msg in warnings)

    def test_syntax_error(self, tmp_path):
        """Syntax errors are caught."""
        skill_dir = tmp_path / "syntax-skill"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text("def broken(:\n")
        issues = _validate_skill_source(skill_dir)
        errors = [msg for level, msg in issues if level == "error"]
        assert any("syntax" in msg.lower() or "Syntax" in msg for msg in errors)

    def test_no_skill_class_error(self, tmp_path):
        """File without Skill subclass is flagged."""
        skill_dir = tmp_path / "no-class-skill"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text("# just a comment\nx = 1\n")
        issues = _validate_skill_source(skill_dir)
        errors = [msg for level, msg in issues if level == "error"]
        assert any("Skill" in msg for msg in errors)

    def test_missing_skill_py(self, tmp_path):
        """Missing skill.py is flagged."""
        skill_dir = tmp_path / "empty-skill"
        skill_dir.mkdir(parents=True)
        issues = _validate_skill_source(skill_dir)
        errors = [msg for level, msg in issues if level == "error"]
        assert any("not found" in msg for msg in errors)

    def test_todo_description_warning(self, tmp_path):
        """TODO in description triggers warning."""
        skill_dir = tmp_path / "todo-skill"
        skill_dir.mkdir(parents=True)
        (skill_dir / "skill.py").write_text(
            "from core.skills.base import Skill, SkillInput, SkillOutput\n"
            "from pydantic import Field\n"
            'class I(SkillInput):\n    query: str = Field(description="q")\n'
            'class O(SkillOutput):\n    data: str = ""\n'
            "class TodoSkill(Skill[I, O]):\n"
            '    name = "todo"\n    version = "1.0.0"\n'
            '    description = "TODO: fill this in"\n'
            "    async def execute(self, input):\n"
            "        return O(success=True)\n"
        )
        issues = _validate_skill_source(skill_dir)
        warnings = [msg for level, msg in issues if level == "warning"]
        assert any("TODO" in msg or "description" in msg for msg in warnings)

    def test_validate_command_not_found(self, tmp_path, monkeypatch):
        """validate command shows error for nonexistent skill."""
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="validate nonexistent")
        assert "not found" in buf.getvalue()

    def test_validate_command_no_name(self, tmp_path, monkeypatch):
        """validate command shows usage when no name given."""
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="validate")
        assert "Usage" in buf.getvalue()


# ============================================================================
# /skill example
# ============================================================================


class TestSkillExample:
    """Tests for /skill example command."""

    def test_shows_example(self, tmp_path, monkeypatch):
        """example command shows complete skill example."""
        monkeypatch.chdir(tmp_path)
        console, buf = _console()
        cmd_skill(console, cmd_arg="example")
        output = buf.getvalue()
        assert "StockInfo" in output or "stock_info" in output
        assert "async def execute" in output
        assert "Field(description=" in output


# ============================================================================
# Template generation
# ============================================================================


class TestGenerateSkillTemplate:
    """Tests for _generate_skill_template."""

    def test_generates_valid_python(self, tmp_path):
        """Generated template is valid Python that can be imported."""
        template = _generate_skill_template("my_tool", "MyTool")
        skill_py = tmp_path / "skill.py"
        skill_py.write_text(template)

        import importlib.util

        spec = importlib.util.spec_from_file_location("_test_skill", skill_py)
        assert spec is not None
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)

        # Should have the expected classes
        assert hasattr(mod, "MyToolInput")
        assert hasattr(mod, "MyToolOutput")
        assert hasattr(mod, "MyToolSkill")

    def test_template_passes_validation(self, tmp_path):
        """Generated template passes _validate_skill_source with no errors."""
        template = _generate_skill_template("test_skill", "TestSkill")
        skill_dir = tmp_path / "test-skill"
        skill_dir.mkdir()
        (skill_dir / "skill.py").write_text(template)

        issues = _validate_skill_source(skill_dir)
        errors = [msg for level, msg in issues if level == "error"]
        assert not errors, f"Template has errors: {errors}"

    def test_template_is_loadable(self, tmp_path):
        """Generated skill can be loaded by SkillLoader."""
        template = _generate_skill_template("loadable", "Loadable")
        skill_dir = tmp_path / "loadable"
        skill_dir.mkdir()
        (skill_dir / "skill.py").write_text(template)

        from core.skills.loader import SkillLoader

        skills = SkillLoader.discover([tmp_path])
        assert len(skills) == 1
        assert skills[0].skill.name == "loadable"

    def test_template_has_field_descriptions(self):
        """Generated template includes Field(description=...)."""
        template = _generate_skill_template("desc_test", "DescTest")
        assert "Field(description=" in template

    def test_template_has_async_execute(self):
        """Generated template has async execute method."""
        template = _generate_skill_template("async_test", "AsyncTest")
        assert "async def execute" in template

    def test_template_has_try_except(self):
        """Generated template wraps logic in try/except."""
        template = _generate_skill_template("safe_test", "SafeTest")
        assert "try:" in template
        assert "except Exception" in template
        assert "success=False" in template


# ============================================================================
# Enhanced dev context
# ============================================================================


class TestEnhancedDevContext:
    """Tests for enhanced _build_skill_dev_context."""

    def test_includes_framework_guide(self, tmp_path):
        """Dev context includes comprehensive framework guide."""
        skill_dir = tmp_path / "test-skill"
        skill_dir.mkdir()
        (skill_dir / "skill.py").write_text("# code")

        ctx = _build_skill_dev_context("test_skill", skill_dir)

        # Should include key framework concepts
        assert "SkillInput" in ctx
        assert "SkillOutput" in ctx
        assert "async def execute" in ctx
        assert "Field(description=" in ctx
        assert "success=False" in ctx

    def test_includes_common_mistakes(self, tmp_path):
        """Dev context includes common mistakes to avoid."""
        skill_dir = tmp_path / "test-skill"
        skill_dir.mkdir()
        (skill_dir / "skill.py").write_text("# code")

        ctx = _build_skill_dev_context("test_skill", skill_dir)

        # Should warn about common mistakes
        assert "raise" in ctx.lower() or "NEVER raise" in ctx
        assert "async" in ctx

    def test_includes_example_patterns(self, tmp_path):
        """Dev context includes example patterns."""
        skill_dir = tmp_path / "test-skill"
        skill_dir.mkdir()
        (skill_dir / "skill.py").write_text("# code")

        ctx = _build_skill_dev_context("test_skill", skill_dir)

        # Should include practical examples
        assert "httpx" in ctx or "akshare" in ctx or "API" in ctx
