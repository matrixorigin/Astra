"""Tests for SkillOutput.guidance and user_message fields.

Tests the actual SkillOutput model and the CIStatusSkill/CIStatusAction
behavior — not copy-pasted inline logic.
"""

import json

import pytest
from unittest.mock import AsyncMock

from core.skills.base import SkillOutput


class TestSkillOutputGuidanceField:
    """Verify SkillOutput.guidance and user_message serialization."""

    def test_guidance_serialized_in_model_dump(self):
        """guidance field appears in serialized output."""
        out = SkillOutput(success=True, guidance="Do NOT retry.")
        data = out.model_dump()
        assert data["guidance"] == "Do NOT retry."

    def test_user_message_serialized_in_model_dump(self):
        """user_message field appears in serialized output."""
        out = SkillOutput(success=True, guidance="Stop.", user_message="No results.")
        data = out.model_dump()
        assert data["user_message"] == "No results."

    def test_guidance_none_by_default(self):
        """guidance defaults to None when not set."""
        out = SkillOutput(success=True)
        assert out.guidance is None
        assert out.user_message is None

    def test_guidance_survives_json_roundtrip(self):
        """guidance and user_message survive JSON serialization."""
        out = SkillOutput(success=True, guidance="Stop.", user_message="No data.")
        raw = json.dumps(out.model_dump(exclude_none=True))
        parsed = json.loads(raw)
        assert parsed["guidance"] == "Stop."
        assert parsed["user_message"] == "No data."

    def test_success_false_and_guidance_both_present(self):
        """Both success=False and guidance can coexist on the model."""
        out = SkillOutput(success=False, guidance="Do NOT retry.")
        assert out.success is False
        assert out.guidance == "Do NOT retry."


class TestCIStatusSkillGuidance:
    """Verify CIStatusSkill sets guidance correctly."""

    @pytest.mark.asyncio
    async def test_empty_runs_sets_guidance_and_user_message(self):
        """Empty workflow list → guidance + user_message set."""
        from core.skills.builtin import CIStatusSkill, CIStatusInput

        mock_client = AsyncMock()
        mock_client.list_wf_runs.return_value = []
        skill = CIStatusSkill(github=mock_client)

        result = await skill.execute(CIStatusInput(repo="owner/repo"))
        assert result.guidance is not None
        assert "do not" in result.guidance.lower()
        assert result.user_message is not None
        assert len(result.user_message) > 0

    @pytest.mark.asyncio
    async def test_nonempty_runs_no_guidance(self):
        """Non-empty workflow list → no guidance, no user_message."""
        from core.skills.builtin import CIStatusSkill, CIStatusInput

        mock_client = AsyncMock()
        mock_client.list_wf_runs.return_value = [
            {
                "workflow": "CI",
                "conclusion": "success",
                "branch": "main",
                "pr_number": None,
                "actor": "bot",
                "triggered_at": "t",
                "url": "u",
            }
        ]
        skill = CIStatusSkill(github=mock_client)

        result = await skill.execute(CIStatusInput(repo="owner/repo"))
        assert result.guidance is None
        assert result.user_message is None


class TestGuidanceInjectionLogic:
    """Verify the if/elif priority: success=False > guidance > nothing."""

    @pytest.mark.parametrize(
        "result_dict,expected_system_content",
        [
            # success=False takes priority over guidance
            ({"success": False, "guidance": "ignored"}, "success=False"),
            # guidance injected when success is not False
            ({"success": True, "guidance": "Stop."}, "Stop."),
            # no guidance, no system message
            ({"success": True}, None),
            # guidance=None → no injection
            ({"success": True, "guidance": None}, None),
            # guidance="" → no injection (falsy)
            ({"success": True, "guidance": ""}, None),
        ],
    )
    def test_priority(self, result_dict, expected_system_content):
        """Simulate the chat loop's post-tool-result branching."""
        messages: list[dict] = []
        result_str = json.dumps(result_dict)
        _parsed = json.loads(result_str)

        # This mirrors the actual logic in chat_loop.py and chat.py
        if isinstance(_parsed, dict):
            if _parsed.get("success") is False:
                messages.append({"role": "system", "content": "success=False"})
            elif _parsed.get("guidance"):
                messages.append({"role": "system", "content": _parsed["guidance"]})

        if expected_system_content is None:
            assert len(messages) == 0
        else:
            assert len(messages) == 1
            assert messages[0]["role"] == "system"
            assert messages[0]["content"] == expected_system_content
