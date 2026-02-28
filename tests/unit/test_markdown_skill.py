"""Tests for MarkdownSkill — wraps SKILL.md as a Skill."""

import pytest

from core.skills.markdown_skill import MarkdownSkill, MarkdownSkillInput, MarkdownSkillOutput
from core.skills.skill_md import SkillMd


@pytest.fixture
def spec():
    return SkillMd(name="test_skill", description="A test", body="Do the thing.", version="2.0.0")


@pytest.fixture
def skill(spec):
    return MarkdownSkill(spec)


class TestMarkdownSkill:
    def test_attributes(self, skill):
        assert skill.name == "test_skill"
        assert skill.version == "2.0.0"
        assert skill.description == "A test"

    @pytest.mark.asyncio
    async def test_execute_returns_body(self, skill):
        out = await skill.execute(MarkdownSkillInput(query="hello"))
        assert isinstance(out, MarkdownSkillOutput)
        assert out.instructions == "Do the thing."
        assert out.result == "Do the thing."
        assert out.success is True

    def test_openai_schema(self, skill):
        schema = skill.to_openai_schema()
        assert schema["type"] == "function"
        assert schema["function"]["name"] == "test_skill"
        assert "query" in schema["function"]["parameters"]["properties"]
