"""Tests for cli/tools/base.py — EdgeTool, SideEffect, resolve_side_effect."""

import pytest

from cli.tools.base import EdgeTool, SideEffect, resolve_side_effect, _SIDE_EFFECT_MAP
from core.skills.base import SideEffectCategory, SideEffectProfile


class TestSideEffect:
    def test_values(self):
        assert SideEffect.READ == "read"
        assert SideEffect.WRITE == "write"
        assert SideEffect.EXECUTE == "execute"


class TestResolveSideEffect:
    def test_edge_tool_with_side_effect(self):
        class FakeTool:
            side_effect = SideEffect.WRITE
        assert resolve_side_effect(FakeTool()) == SideEffect.WRITE

    def test_typed_skill_with_profile(self):
        """Typed skill has side_effect_profile but no side_effect attr."""
        class FakeSkill:
            side_effect_profile = SideEffectProfile(category=SideEffectCategory.EXECUTE)
        assert resolve_side_effect(FakeSkill()) == SideEffect.EXECUTE

    def test_fallback_to_read(self):
        """Object with neither side_effect nor side_effect_profile → READ."""
        class Empty:
            pass
        assert resolve_side_effect(Empty()) == SideEffect.READ

    def test_profile_with_destructive_category(self):
        """DESTRUCTIVE maps to EXECUTE — destructive ops need execute-level gates."""
        class FakeSkill:
            side_effect_profile = SideEffectProfile(category=SideEffectCategory.DESTRUCTIVE)
        assert resolve_side_effect(FakeSkill()) == SideEffect.EXECUTE


class TestEdgeTool:
    def test_version_default(self):
        assert EdgeTool.version == "1.0.0"

    def test_side_effect_profile_property(self):
        """side_effect_profile derived from side_effect."""
        from typing import Any

        class DummyTool(EdgeTool):
            name = "dummy"
            description = "test"
            parameters: dict[str, Any] = {"type": "object", "properties": {}}
            side_effect = SideEffect.WRITE

            async def execute(self, **kwargs: Any) -> str:
                return "ok"

        tool = DummyTool()
        assert tool.side_effect_profile.category == SideEffectCategory.WRITE

    def test_to_openai_schema(self):
        from typing import Any

        class DummyTool(EdgeTool):
            name = "test_tool"
            description = "A test"
            parameters: dict[str, Any] = {
                "type": "object",
                "properties": {"x": {"type": "string"}},
            }
            side_effect = SideEffect.READ

            async def execute(self, **kwargs: Any) -> str:
                return ""

        schema = DummyTool().to_openai_schema()
        assert schema["type"] == "function"
        assert schema["function"]["name"] == "test_tool"
        assert "x" in schema["function"]["parameters"]["properties"]

    def test_requirements_default(self):
        from core.skills.base import RuntimeRequirement
        assert RuntimeRequirement.FILESYSTEM in EdgeTool.requirements.runtime
        assert EdgeTool.requirements.llm_required is False
