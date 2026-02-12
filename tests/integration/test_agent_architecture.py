"""Integration tests for Agent Architecture."""

import asyncio
import unittest
from unittest.mock import MagicMock, patch

from core.agent.chat_loop import ChatLoop
from core.agent.executor import AgentExecutor
from core.agent.selector import AgentSkillSelector
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
        # Simple mock validation
        # In real world, we would validate user_id/session_id
        return MockInput(
            user_id=input_data.get("user_id", "test_user"),
            session_id=input_data.get("session_id", "test_session"),
            param=input_data.get("param", "default"),
        )

    async def execute(self, input: SkillInput) -> SkillOutput:
        return SkillOutput(success=True, result=f"Executed with {input.param}")


class TestAgentArchitecture(unittest.TestCase):
    def setUp(self):
        self.db = MagicMock()
        self.registry = MagicMock()
        self.llm_client = MagicMock()
        self.event_logger = MagicMock()
        self.event_logger.create_user_query.return_value.event_id = "user_event_1"
        self.event_logger.create_user_query.return_value.causal_chain_id = "chain_1"

        # Setup Registry to return our mock skill
        self.mock_skill = MockSkill()
        self.registry.get.return_value = self.mock_skill

        # Create context_manager and firewall mocks
        self.context_manager = MagicMock()
        self.context_manager.build_context.return_value = MagicMock(
            system_prompt="Test prompt",
            skill_definitions=[],
            selected_events=[],
            code_context=[],
            documentation=[],
            total_tokens=100,
            token_budget={},
            assembly_time_ms=10,
            relevance_scores={},
            task_type="general",
        )
        self.context_manager.save_snapshot.return_value = "snapshot_123"

        self.firewall = MagicMock()
        self.firewall.verify_response.return_value = MagicMock(
            safe_to_deliver=True,
            confidence_score=0.9,
            claims_verified=0,
            claims_failed=0,
            contradictions=[],
            warnings=[],
        )

        self.selector = AgentSkillSelector(self.db, self.llm_client)
        self.executor = AgentExecutor(self.db, self.registry, MockMode.PRODUCTION)
        self.chat_loop = ChatLoop(
            selector=self.selector,
            executor=self.executor,
            llm_client=self.llm_client,
            event_logger=self.event_logger,
            context_manager=self.context_manager,
            firewall=self.firewall,
        )

        # Mock the selector's get_tools_schema method
        self.selector.selector.get_tools_schema = MagicMock(
            return_value=[
                {
                    "type": "function",
                    "function": {
                        "name": "test_skill",
                        "parameters": {
                            "type": "object",
                            "properties": {"param": {"type": "string"}},
                        },
                    },
                }
            ]
        )

    def test_selector_delegation(self):
        """Test that selector delegates to underlying selector."""
        selector = AgentSkillSelector(self.db, self.llm_client)
        # Just verify it doesn't crash
        result = selector.select_skills("query", {})
        assert isinstance(result, list)

    def test_executor_execution(self):
        """Test that executor uses ToolMockingLayer."""
        # We need to mock ToolMockingLayer because it might do DB calls or validation
        with patch("core.agent.executor.ToolMockingLayer") as mock_layer:
            executor = AgentExecutor(self.db, self.registry, MockMode.PRODUCTION)
            executor.execute_skill("test_skill", {"param": "value"}, "session_1", "parent_1")
            mock_layer.return_value.execute.assert_called()

    def test_chat_loop_flow(self):
        """Test the full chat loop flow with skills."""
        # Mock selector to return a tool call
        self.selector.select_skills = MagicMock(
            return_value=[{"function": {"name": "test_skill", "arguments": '{"param": "value"}'}}]
        )

        # Mock executor to return a result
        self.executor.execute_skill = MagicMock(
            return_value=SkillOutput(success=True, result="Skill Result")
        )

        # Mock LLM to return tool calls first, then final response
        mock_tool_response = MagicMock()
        mock_tool_response.content = None
        mock_tool_response.tool_calls = [
            MagicMock(
                id="tc_1",
                type="function",
                function=MagicMock(name="test_skill", arguments='{"param": "value"}'),
            )
        ]

        mock_final_response = MagicMock()
        mock_final_response.content = "Final Answer"

        # First call returns tool calls, second call returns final answer
        self.llm_client.chat_with_tools.side_effect = [
            {
                "content": None,
                "tool_calls": [
                    {
                        "id": "tc_1",
                        "function": {"name": "test_skill", "arguments": '{"param": "value"}'},
                    }
                ],
            },
            {"content": "Final Answer", "tool_calls": []},
        ]
        self.llm_client.chat.return_value = mock_final_response

        # Run loop
        result = asyncio.run(self.chat_loop.run_step("User Input", "session_1", "user_1"))

        # Verify
        self.event_logger.create_user_query.assert_called_with(
            user_id="user_1", session_id="session_1", content="User Input"
        )
        # selector.selector.get_tools_schema is called (not select_skills)
        self.selector.selector.get_tools_schema.assert_called_with(
            query="User Input", max_candidates=5
        )
        self.executor.execute_skill.assert_called_with(
            skill_name="test_skill",
            params={"param": "value"},
            session_id="session_1",
            parent_event_id="user_event_1",
        )
        self.assertEqual(result, "Final Answer")


if __name__ == "__main__":
    unittest.main()
