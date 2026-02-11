"""Unit tests for multi-agent collaboration features."""

import unittest
from unittest.mock import MagicMock, AsyncMock
from core.agent.agent_registry import AgentRegistry, AgentProfile
from core.skills.delegation import DelegateTaskSkill, DelegateTaskInput, DelegateTaskOutput


class TestAgentRegistry(unittest.TestCase):
    """Test AgentRegistry functionality."""

    def setUp(self):
        self.registry = AgentRegistry()

    def test_register_agent(self):
        """Test registering an agent profile."""
        profile = AgentProfile(
            agent_id="test_agent", system_prompt="Test prompt", skill_filter=["skill1"]
        )
        self.registry.register(profile)

        retrieved = self.registry.get("test_agent")
        self.assertIsNotNone(retrieved)
        self.assertEqual(retrieved.agent_id, "test_agent")

    def test_get_nonexistent_agent(self):
        """Test getting a non-existent agent."""
        result = self.registry.get("nonexistent")
        self.assertIsNone(result)

    def test_list_agents(self):
        """Test listing all agents."""
        self.registry.register(AgentProfile(agent_id="agent1", system_prompt="p1"))
        self.registry.register(AgentProfile(agent_id="agent2", system_prompt="p2"))

        agents = self.registry.list_agents()
        self.assertEqual(len(agents), 2)

    def test_unregister_agent(self):
        """Test unregistering an agent."""
        self.registry.register(AgentProfile(agent_id="test", system_prompt="p"))
        result = self.registry.unregister("test")

        self.assertTrue(result)
        self.assertIsNone(self.registry.get("test"))


class TestDelegateTaskSkill(unittest.IsolatedAsyncioTestCase):
    """Test DelegateTaskSkill functionality."""

    def setUp(self):
        self.registry = AgentRegistry()
        self.registry.register(
            AgentProfile(
                agent_id="code_reviewer",
                system_prompt="You are a code reviewer.",
                skill_filter=["analyze_code"],
            )
        )

        # Mock chat_loop_factory
        self.mock_loop = AsyncMock()
        self.mock_loop.run_step = AsyncMock(return_value="Review result")
        self.loop_factory = MagicMock(return_value=self.mock_loop)

        self.skill = DelegateTaskSkill(
            agent_registry=self.registry, chat_loop_factory=self.loop_factory
        )

    async def test_execute_valid_agent(self):
        """Test executing delegation to a valid agent."""
        input_data = DelegateTaskInput(
            agent_id="code_reviewer",
            task="Review this PR",
            session_id="session_1",
            user_id="user_1",
        )

        result = await self.skill.execute(input_data)

        self.assertTrue(result.success)
        self.assertEqual(result.agent_id, "code_reviewer")
        self.assertEqual(result.result, "Review result")
        self.loop_factory.assert_called_once()

    async def test_execute_invalid_agent(self):
        """Test executing delegation to an invalid agent."""
        input_data = DelegateTaskInput(
            agent_id="nonexistent_agent",
            task="Review this PR",
            session_id="session_1",
            user_id="user_1",
        )

        result = await self.skill.execute(input_data)

        self.assertFalse(result.success)
        self.assertIn("not found", result.result)

    async def test_execute_with_context(self):
        """Test executing delegation with context."""
        input_data = DelegateTaskInput(
            agent_id="code_reviewer",
            task="Review this PR",
            context="Additional context",
            session_id="session_1",
            user_id="user_1",
        )

        result = await self.skill.execute(input_data)

        self.assertTrue(result.success)
        # Verify the loop was created with the correct system prompt
        self.loop_factory.assert_called_once_with(system_prompt="You are a code reviewer.")


if __name__ == "__main__":
    unittest.main()
