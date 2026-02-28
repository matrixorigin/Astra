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

    def test_side_effect_is_read(self, skill):
        """MarkdownSkill must expose side_effect for the permission system."""
        from cli.tools.base import SideEffect
        assert skill.side_effect == SideEffect.READ
        assert skill.side_effect.value == "read"

    def test_works_in_tool_router(self, skill):
        """MarkdownSkill registered in ToolRouter must be introspectable."""
        from cli.tools.router import ToolRouter
        router = ToolRouter()
        router.register(skill)
        tools = router.list_tools()
        assert len(tools) == 1
        # This is the exact access pattern used by edge_chat_loop (permissions)
        # and GetAgentInfoTool (introspection). Both crashed before the fix.
        assert tools[0].side_effect.value == "read"

    @pytest.mark.asyncio
    async def test_execute_through_router(self, skill):
        """ToolRouter dispatches to MarkdownSkill via validate_input → execute(input)."""
        from cli.tools.router import ToolRouter, ToolCall
        router = ToolRouter()
        router.register(skill)
        results = await router.execute([ToolCall(id="tc1", name="test_skill", arguments={"query": "hello"})])
        assert len(results) == 1
        assert not results[0].error
        assert "Do the thing." in results[0].result

    @pytest.mark.asyncio
    async def test_execute_through_router_empty_query(self, skill):
        """MarkdownSkill works with empty query (query has default)."""
        from cli.tools.router import ToolRouter, ToolCall
        router = ToolRouter()
        router.register(skill)
        results = await router.execute([ToolCall(id="tc1", name="test_skill", arguments={})])
        assert not results[0].error
