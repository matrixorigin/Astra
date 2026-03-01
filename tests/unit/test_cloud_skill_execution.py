"""Tests for _execute_cloud_skill and _get_cloud_skill_schemas in /chat/turn."""

import json
import os
import pytest
from unittest.mock import AsyncMock, MagicMock

os.environ.setdefault("TOKEN_ENCRYPTION_KEY", "test-key-" + "x" * 32)
os.environ.setdefault("JWT_SECRET_KEY", "test-jwt-secret-" + "x" * 32)

from core.skills.base import Skill, SkillInput, SkillOutput
from fastapi.testclient import TestClient
from api.main import app
from tests.conftest import parse_sse_events, fake_llm_stream, get_auth_headers


@pytest.fixture
def client():
    return TestClient(app)


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


# ── Cloud skill event recording ───────────────────────────────────────────

class TestCloudSkillEventRecording:
    """Cloud skill executions in /chat/turn must be recorded in agent_events."""

    def test_cloud_skill_recorded_in_decision_trace(self, client, db):
        """After /chat/turn calls a cloud skill, decision-trace shows it in tool_usage."""
        import os
        from unittest.mock import patch, MagicMock
        from tests.conftest import get_auth_headers, parse_sse_events, fake_llm_stream

        headers = get_auth_headers(client, db, username="dt_user", user_id="dt_uid", email="dt@t.com")

        # LLM returns a tool_call for list_prs, then a text answer
        call_stream = fake_llm_stream([
            {"type": "tool_call", "data": {
                "id": "tc1", "function": {"name": "list_prs", "arguments": '{"repo":"o/r","limit":1}'}}},
        ])
        answer_stream = fake_llm_stream([{"type": "text", "content": "Here are PRs"}])

        call_count = {"n": 0}
        def side_effect(*a, **kw):
            call_count["n"] += 1
            return call_stream if call_count["n"] == 1 else answer_stream

        mock_skill_output = MagicMock()
        mock_skill_output.model_dump.return_value = {"success": True, "prs": []}

        with patch("core.llm.client.LLMClient.chat_with_tools_stream", side_effect=side_effect), \
             patch("api.routers.chat._get_shared_skill_registry") as mock_reg_fn, \
             patch("api.routers.chat._get_cloud_skill_schemas", return_value=[
                 {"type": "function", "function": {"name": "list_prs", "description": "list prs", "parameters": {}}}
             ]):
            mock_reg = MagicMock()
            mock_skill = MagicMock()
            mock_skill.name = "list_prs"
            mock_skill.validate_input.return_value = MagicMock()

            async def fake_execute(inp):
                return mock_skill_output
            mock_skill.execute = fake_execute
            mock_skill._input_cls = MagicMock()
            mock_reg.get.return_value = mock_skill
            mock_reg._skills = {"list_prs": mock_skill}
            mock_reg_fn.return_value = mock_reg

            resp = client.post("/chat/turn", json={
                "messages": [{"role": "user", "content": "list prs"}],
                "edge_tools": [{"type": "function", "function": {"name": "bash", "description": "sh", "parameters": {}}}],
            }, headers=headers)
            assert resp.status_code == 200

            events = parse_sse_events(resp.text)
            session_id = next((e["session_id"] for e in events if e.get("type") == "session_info"), None)
            assert session_id

        # Now check decision-trace
        dt_resp = client.get(f"/chat/session/{session_id}/decision-trace?question=pr", headers=headers)
        assert dt_resp.status_code == 200
        usage = dt_resp.json().get("tool_usage_counts", {})
        assert "list_prs" in usage, f"list_prs not in tool_usage: {usage}"
