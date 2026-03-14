"""Tests for SkillLoader — discover and load skills from directories."""

import textwrap

import pytest

from core.skills.loader import SkillLoader, LocalSkill


@pytest.fixture
def skills_dir(tmp_path):
    """Create a temp skills directory with various skill types."""
    return tmp_path / ".mo-agent" / "skills"


def _write_skill_md(skill_dir, name="test_skill", description="A test skill", body="Do stuff."):
    skill_dir.mkdir(parents=True, exist_ok=True)
    (skill_dir / "SKILL.md").write_text(
        textwrap.dedent(f"""\
        ---
        name: {name}
        version: 1.0.0
        description: {description}
        ---
        {body}
    """)
    )


def _write_skill_py(skill_dir, name="test_skill", description="A typed skill"):
    skill_dir.mkdir(parents=True, exist_ok=True)
    (skill_dir / "skill.py").write_text(
        textwrap.dedent(f"""\
        from core.skills.base import Skill, SkillInput, SkillOutput
        from pydantic import Field

        class TestInput(SkillInput):
            query: str = ""

        class TestOutput(SkillOutput):
            answer: str = ""

        class TestSkill(Skill[TestInput, TestOutput]):
            name = "{name}"
            version = "1.0.0"
            description = "{description}"

            async def execute(self, input: TestInput) -> TestOutput:
                return TestOutput(success=True, answer="real data for: " + input.query)
    """)
    )


class TestSkillLoaderDiscover:
    def test_loads_markdown_skill(self, skills_dir):
        d = skills_dir / "my-skill"
        _write_skill_md(d, name="my_skill")
        results = SkillLoader.discover([skills_dir])
        assert len(results) == 1
        assert results[0].skill.name == "my_skill"
        assert type(results[0].skill).__name__ == "MarkdownSkill"

    def test_prefers_skill_py_over_skill_md(self, skills_dir):
        """When both skill.py and SKILL.md exist, skill.py wins."""
        d = skills_dir / "my-skill"
        _write_skill_md(d, name="my_skill", description="markdown version")
        _write_skill_py(d, name="my_skill", description="typed version")
        results = SkillLoader.discover([skills_dir])
        assert len(results) == 1
        assert results[0].skill.name == "my_skill"
        assert type(results[0].skill).__name__ == "TestSkill"
        assert results[0].skill.description == "typed version"

    def test_skill_py_only_no_skill_md(self, skills_dir):
        """skill.py works even without SKILL.md."""
        d = skills_dir / "typed-only"
        _write_skill_py(d, name="typed_only")
        results = SkillLoader.discover([skills_dir])
        assert len(results) == 1
        assert results[0].skill.name == "typed_only"

    def test_empty_dir_skipped(self, skills_dir):
        (skills_dir / "empty-skill").mkdir(parents=True)
        results = SkillLoader.discover([skills_dir])
        assert len(results) == 0

    def test_bad_skill_py_falls_back_to_md(self, skills_dir):
        """If skill.py fails to load, fall back to SKILL.md."""
        d = skills_dir / "bad-py"
        _write_skill_md(d, name="bad_py")
        d.mkdir(parents=True, exist_ok=True)
        (d / "skill.py").write_text("raise ImportError('broken')")
        results = SkillLoader.discover([skills_dir])
        assert len(results) == 1
        assert type(results[0].skill).__name__ == "MarkdownSkill"


class TestSkillOutputResult:
    def test_result_is_optional(self):
        from core.skills.base import SkillOutput

        o = SkillOutput(success=True)
        assert o.result is None

    def test_subclass_without_result(self):
        """Subclass with custom fields should not require result."""
        from core.skills.base import SkillOutput

        class MyOutput(SkillOutput):
            data: dict = {}

        o = MyOutput(success=True, data={"key": "value"})
        assert o.data == {"key": "value"}
        assert o.result is None


class TestRouterSkillSerialization:
    @pytest.mark.asyncio
    async def test_typed_skill_output_serialized_as_json(self, skills_dir):
        """Router serializes full SkillOutput (all fields) as JSON."""
        from cli.tools.router import ToolRouter, ToolCall

        d = skills_dir / "json-skill"
        _write_skill_py(d, name="json_skill")
        results = SkillLoader.discover([skills_dir])
        router = ToolRouter()
        router.register(results[0].skill)

        [r] = await router.execute(
            [ToolCall(id="t1", name="json_skill", arguments={"query": "test"})]
        )
        assert not r.error
        import json

        data = json.loads(r.result)
        assert data["success"] is True
        assert data["answer"] == "real data for: test"

    @pytest.mark.asyncio
    async def test_skill_error_returned_not_raised(self, skills_dir):
        """Skill returning success=False should not cause router exception."""
        from cli.tools.router import ToolRouter, ToolCall

        d = skills_dir / "err-skill"
        d.mkdir(parents=True, exist_ok=True)
        (d / "skill.py").write_text(
            textwrap.dedent("""\
            from core.skills.base import Skill, SkillInput, SkillOutput

            class ErrInput(SkillInput):
                query: str = ""

            class ErrOutput(SkillOutput):
                pass

            class ErrSkill(Skill[ErrInput, ErrOutput]):
                name = "err_skill"
                version = "1.0.0"
                description = "always fails"

                async def execute(self, input: ErrInput) -> ErrOutput:
                    return ErrOutput(success=False, error="network timeout")
        """)
        )
        results = SkillLoader.discover([skills_dir])
        router = ToolRouter()
        router.register(results[0].skill)

        [r] = await router.execute([ToolCall(id="t1", name="err_skill", arguments={})])
        # Should NOT be an exception — error is in the JSON payload
        assert not r.error
        import json

        data = json.loads(r.result)
        assert data["success"] is False
        assert "network timeout" in data["error"]
