"""Integration tests for Agent Architecture."""

import asyncio
import unittest
from unittest.mock import MagicMock, Mock, patch

from sqlalchemy.orm import Session

from core.agent.chat_loop import ChatLoop
from core.agent.executor import AgentExecutor
from core.skills.pipeline import SkillPipeline, ToolsResult
from core.skills.base import (
    AccessScope,
    RepoType,
    SideEffectCategory,
    SideEffectProfile,
    Skill,
    SkillInput,
    SkillOutput,
    SkillRequirement,
)
from core.skills.mocking import MockMode


class MockInput(SkillInput):
    param: str = "default"


class MockSkill(Skill):
    name: str = "test_skill"
    version: str = "1.0.0"
    description: str = "Test skill"
    requirements: SkillRequirement = SkillRequirement(
        repo_types=[RepoType.CODE], min_access=AccessScope.READ
    )
    side_effect_profile: SideEffectProfile = SideEffectProfile(category=SideEffectCategory.READ)

    def validate_input(self, input_data: dict) -> SkillInput:
        return MockInput(
            user_id=input_data.get("user_id", "test_user"),
            session_id=input_data.get("session_id", "test_session"),
            param=input_data.get("param", "default"),
        )

    async def execute(self, input: SkillInput) -> SkillOutput:
        return SkillOutput(success=True, result=f"Executed with {input.param}")


class TestAgentArchitecture(unittest.TestCase):
    def setUp(self):
        self.db = Mock(spec=Session)
        self.db.query.return_value.filter.return_value.all.return_value = []

        self.registry = MagicMock()
        self.llm_client = MagicMock()
        self.event_logger = MagicMock()
        self.event_logger.create_user_query.return_value.event_id = "user_event_1"
        self.event_logger.create_user_query.return_value.causal_chain_id = "chain_1"

        self.mock_skill = MockSkill()
        self.registry.get.return_value = self.mock_skill

        self.context_manager = MagicMock()
        self.context_manager.build_context.return_value = MagicMock(
            system_prompt="Test prompt", skill_definitions=[], selected_events=[],
            code_context=[], documentation=[], total_tokens=100, token_budget={},
            assembly_time_ms=10, relevance_scores={}, task_type="general",
        )
        self.context_manager.save_snapshot.return_value = "snapshot_123"

        self.firewall = MagicMock()
        self.firewall.verify_response.return_value = MagicMock(
            safe_to_deliver=True, confidence_score=0.9,
            claims_verified=0, claims_failed=0, contradictions=[], warnings=[],
        )

        # Use SkillPipeline with mocked internals
        self.pipeline = SkillPipeline(self.db, self.llm_client, audit=False, learning=False)
        self.pipeline._modern.get_tools_schema = MagicMock(return_value=[
            {
                "type": "function",
                "function": {
                    "name": "test_skill",
                    "parameters": {"type": "object", "properties": {"param": {"type": "string"}}},
                },
            }
        ])

        self.executor = AgentExecutor(self.db, self.registry, MockMode.PRODUCTION)
        self.chat_loop = ChatLoop(
            selector=self.pipeline,
            executor=self.executor,
            llm_client=self.llm_client,
            event_logger=self.event_logger,
            context_manager=self.context_manager,
            firewall=self.firewall,
        )

    def test_executor_execution(self):
        """Test that executor uses ToolMockingLayer."""
        with patch("core.agent.executor.ToolMockingLayer") as mock_layer:
            executor = AgentExecutor(self.db, self.registry, MockMode.PRODUCTION)
            executor.execute_skill("test_skill", {"param": "value"}, "session_1", "parent_1")
            mock_layer.return_value.execute.assert_called()

    def test_chat_loop_flow(self):
        """Test the full chat loop flow with skills."""
        self.executor.execute_skill = MagicMock(
            return_value=SkillOutput(success=True, result="Skill Result")
        )

        self.llm_client.chat_with_tools.side_effect = [
            {
                "content": None,
                "tool_calls": [
                    {"id": "tc_1", "function": {"name": "test_skill", "arguments": '{"param": "value"}'}},
                ],
            },
            {"content": "Final Answer", "tool_calls": []},
        ]

        result = asyncio.run(self.chat_loop.run_step("User Input", "session_1", "user_1"))

        self.event_logger.create_user_query.assert_called_with(
            user_id="user_1", session_id="session_1", content="User Input"
        )
        self.executor.execute_skill.assert_called_with(
            skill_name="test_skill", params={"param": "value"},
            session_id="session_1", parent_event_id="user_event_1",
        )
        self.assertEqual(result, "Final Answer")


if __name__ == "__main__":
    unittest.main()
