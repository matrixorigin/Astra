"""Tests for _execute_cloud_skill and _get_cloud_skill_schemas in /chat/turn."""

import json
import pytest
from unittest.mock import AsyncMock, MagicMock

from core.skills.base import Skill, SkillInput, SkillOutput


# ── Fixtures ──────────────────────────────────────────────────────────────

class DummyInput(SkillInput):
    repo: str = ""

class DummyOutput(SkillOutput):
    items: list = []

class DummySkill(Skill[DummyInput, DummyOutput]):
    name = "dummy_skill"
    version = "1.0.0"
    description = "test"

    async def execute(self, input: DummyInput) -> DummyOutput:
        if input.repo == "bad/repo":
            raise ValueError("Repository bad/repo not found on GitHub")
        if input.repo == "timeout/repo":
            raise TimeoutError("request timed out")
        if input.repo == "rate/limited":
            raise RuntimeError("rate limit exceeded")
        return DummyOutput(success=True, result="ok", items=[{"repo": input.repo}])


@pytest.fixture
def registry():
    from core.skills.registry import SkillRegistry
    reg = MagicMock(spec=SkillRegistry)
    skill = DummySkill()
    reg.get.return_value = skill
    reg._skills = {"dummy_skill": skill}
    return reg


# ── _execute_cloud_skill ──────────────────────────────────────────────────

class TestExecuteCloudSkill:

    @pytest.mark.asyncio
    async def test_success_returns_json(self, registry):
        from api.routers.chat import _execute_cloud_skill
        result = await _execute_cloud_skill(registry, "dummy_skill", {"repo": "owner/repo"})
        data = json.loads(result)
        assert data["success"] is True
        assert data["items"][0]["repo"] == "owner/repo"

    @pytest.mark.asyncio
    async def test_not_found_skill(self, registry):
        from core.exceptions import SkillNotFoundError
        registry.get.side_effect = SkillNotFoundError("nope")
        from api.routers.chat import _execute_cloud_skill
        result = await _execute_cloud_skill(registry, "nope", {})
        data = json.loads(result)
        assert "not found" in data["error"]

    @pytest.mark.asyncio
    async def test_execution_error_not_retryable(self, registry):
        from api.routers.chat import _execute_cloud_skill
        result = await _execute_cloud_skill(registry, "dummy_skill", {"repo": "bad/repo"})
        data = json.loads(result)
        assert "not found" in data["error"]
        assert data["retryable"] is False

    @pytest.mark.asyncio
    async def test_timeout_error_retryable(self, registry):
        from api.routers.chat import _execute_cloud_skill
        result = await _execute_cloud_skill(registry, "dummy_skill", {"repo": "timeout/repo"})
        data = json.loads(result)
        assert "timed out" in data["error"]
        assert data["retryable"] is True

    @pytest.mark.asyncio
    async def test_rate_limit_error_retryable(self, registry):
        from api.routers.chat import _execute_cloud_skill
        result = await _execute_cloud_skill(registry, "dummy_skill", {"repo": "rate/limited"})
        data = json.loads(result)
        assert data["retryable"] is True


# ── _get_cloud_skill_schemas ──────────────────────────────────────────────

class TestGetCloudSkillSchemas:

    def test_returns_schemas(self, registry):
        from api.routers.chat import _get_cloud_skill_schemas
        schemas = _get_cloud_skill_schemas(registry)
        assert len(schemas) == 1
        assert schemas[0]["function"]["name"] == "dummy_skill"
        assert "repo" in schemas[0]["function"]["parameters"]["properties"]

    def test_skips_versioned_aliases(self, registry):
        from api.routers.chat import _get_cloud_skill_schemas
        registry._skills["dummy_skill@1.0.0"] = registry._skills["dummy_skill"]
        schemas = _get_cloud_skill_schemas(registry)
        assert len(schemas) == 1  # no duplicate
