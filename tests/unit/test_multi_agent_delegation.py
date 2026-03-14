"""Tests for multi-agent delegation enhancements."""

import pytest

from core.agent.agent_registry import AgentProfile, AgentRegistry


class TestAgentProfileExtensions:
    """Test extended AgentProfile fields."""

    def test_basic_profile(self):
        """Test basic agent profile creation."""
        profile = AgentProfile(
            agent_id="test_agent",
            system_prompt="Test prompt",
        )
        assert profile.agent_id == "test_agent"
        assert profile.tier == "user"
        assert not profile.can_delegate
        assert profile.delegate_to == []
        assert profile.triggers == []

    def test_orchestrator_profile(self):
        """Test orchestrator agent profile."""
        profile = AgentProfile(
            agent_id="orchestrator",
            system_prompt="Orchestrate tasks",
            tier="orchestrator",
            can_delegate=True,
            delegate_to=["agent1", "agent2"],
        )
        assert profile.tier == "orchestrator"
        assert profile.can_delegate
        assert len(profile.delegate_to) == 2

    def test_system_agent_profile(self):
        """Test system agent profile with triggers."""
        profile = AgentProfile(
            agent_id="regression_agent",
            system_prompt="Run regression tests",
            tier="system",
            can_delegate=True,
            triggers=["skill_change", "prompt_change"],
        )
        assert profile.tier == "system"
        assert len(profile.triggers) == 2

    def test_user_agent_cannot_have_triggers(self):
        """Test that user agents cannot have triggers."""
        with pytest.raises(ValueError, match="Only system agents can have triggers"):
            AgentProfile(
                agent_id="user_agent",
                system_prompt="User agent",
                tier="user",
                triggers=["some_event"],
            )

    def test_user_agent_cannot_delegate(self):
        """Test that user agents cannot delegate."""
        with pytest.raises(ValueError, match="Only orchestrator and system agents can delegate"):
            AgentProfile(
                agent_id="user_agent",
                system_prompt="User agent",
                tier="user",
                can_delegate=True,
            )


class TestAgentRegistryDelegation:
    """Test delegation validation in AgentRegistry."""

    def setup_method(self):
        """Set up test fixtures."""
        self.registry = AgentRegistry()

    def test_register_with_valid_delegate_to(self):
        """Test registering agent with valid delegate_to references."""
        # Register target agents first
        self.registry.register(AgentProfile(agent_id="agent1", system_prompt="Agent 1"))
        self.registry.register(AgentProfile(agent_id="agent2", system_prompt="Agent 2"))

        # Register orchestrator that delegates to them
        orchestrator = AgentProfile(
            agent_id="orchestrator",
            system_prompt="Orchestrator",
            tier="orchestrator",
            can_delegate=True,
            delegate_to=["agent1", "agent2"],
        )
        self.registry.register(orchestrator)

        assert self.registry.get("orchestrator") is not None

    def test_register_with_invalid_delegate_to(self):
        """Test registering agent with invalid delegate_to references."""
        with pytest.raises(ValueError, match="Delegate target 'nonexistent' not found"):
            self.registry.register(
                AgentProfile(
                    agent_id="orchestrator",
                    system_prompt="Orchestrator",
                    tier="orchestrator",
                    can_delegate=True,
                    delegate_to=["nonexistent"],
                )
            )

    def test_can_delegate_check(self):
        """Test can_delegate permission check."""
        # Register target agents first
        self.registry.register(AgentProfile(agent_id="agent1", system_prompt="Agent 1"))
        self.registry.register(AgentProfile(agent_id="agent2", system_prompt="Agent 2"))

        # Register orchestrator that can delegate to agent1
        self.registry.register(
            AgentProfile(
                agent_id="orchestrator",
                system_prompt="Orchestrator",
                tier="orchestrator",
                can_delegate=True,
                delegate_to=["agent1"],
            )
        )

        # Orchestrator can delegate to agent1
        assert self.registry.can_delegate("orchestrator", "agent1")

        # Orchestrator cannot delegate to agent2 (not in whitelist)
        assert not self.registry.can_delegate("orchestrator", "agent2")

        # agent1 cannot delegate (not an orchestrator)
        assert not self.registry.can_delegate("agent1", "agent2")

    def test_can_delegate_to_anyone(self):
        """Test delegation with empty delegate_to (can delegate to anyone)."""
        self.registry.register(AgentProfile(agent_id="agent1", system_prompt="Agent 1"))
        self.registry.register(
            AgentProfile(
                agent_id="orchestrator",
                system_prompt="Orchestrator",
                tier="orchestrator",
                can_delegate=True,
                delegate_to=[],  # Empty means can delegate to anyone
            )
        )

        # Can delegate to any agent
        assert self.registry.can_delegate("orchestrator", "agent1")
        assert self.registry.can_delegate("orchestrator", "any_agent")


class TestCoordinationPatterns:
    """Test coordination patterns."""

    @pytest.mark.asyncio
    async def test_fan_out_pattern(self):
        """Test fan-out parallel execution pattern."""
        from unittest.mock import AsyncMock, MagicMock

        from core.agent.coordination import CoordinationPatterns, Task
        from core.skills.delegation import DelegateTaskOutput

        # Mock delegation skill
        mock_delegate = MagicMock()
        mock_delegate.execute = AsyncMock(
            return_value=DelegateTaskOutput(
                success=True,
                result="Task completed",
                agent_id="test_agent",
                events_produced=1,
            )
        )

        patterns = CoordinationPatterns(mock_delegate)

        tasks = [
            Task(agent_id="agent1", description="Task 1"),
            Task(agent_id="agent2", description="Task 2"),
            Task(agent_id="agent3", description="Task 3"),
        ]

        results = await patterns.fan_out(tasks, "session1", "user1")

        assert len(results) == 3
        assert all(r.success for r in results)
        assert mock_delegate.execute.call_count == 3

    @pytest.mark.asyncio
    async def test_pipeline_pattern(self):
        """Test pipeline sequential execution pattern."""
        from unittest.mock import AsyncMock, MagicMock

        from core.agent.coordination import CoordinationPatterns, Task
        from core.skills.delegation import DelegateTaskOutput

        # Mock delegation skill with sequential outputs
        outputs = ["Step 1 output", "Step 2 output", "Step 3 output"]
        call_count = 0

        async def mock_execute(input_data):
            nonlocal call_count
            result = outputs[call_count]
            call_count += 1
            return DelegateTaskOutput(
                success=True,
                result=result,
                agent_id=input_data.agent_id,
                events_produced=1,
            )

        mock_delegate = MagicMock()
        mock_delegate.execute = AsyncMock(side_effect=mock_execute)

        patterns = CoordinationPatterns(mock_delegate)

        steps = [
            Task(agent_id="agent1", description="Step 1"),
            Task(agent_id="agent2", description="Step 2"),
            Task(agent_id="agent3", description="Step 3"),
        ]

        result = await patterns.pipeline(steps, "session1", "user1")

        assert result.success
        assert result.output == "Step 3 output"
        assert mock_delegate.execute.call_count == 3

    @pytest.mark.asyncio
    async def test_pipeline_early_termination(self):
        """Test pipeline stops on first failure."""
        from unittest.mock import AsyncMock, MagicMock

        from core.agent.coordination import CoordinationPatterns, Task
        from core.skills.delegation import DelegateTaskOutput

        # Mock delegation skill that fails on second call
        call_count = 0

        async def mock_execute(input_data):
            nonlocal call_count
            call_count += 1
            if call_count == 2:
                return DelegateTaskOutput(
                    success=False,
                    result="Step 2 failed",
                    agent_id=input_data.agent_id,
                    events_produced=1,
                )
            return DelegateTaskOutput(
                success=True,
                result=f"Step {call_count} output",
                agent_id=input_data.agent_id,
                events_produced=1,
            )

        mock_delegate = MagicMock()
        mock_delegate.execute = AsyncMock(side_effect=mock_execute)

        patterns = CoordinationPatterns(mock_delegate)

        steps = [
            Task(agent_id="agent1", description="Step 1"),
            Task(agent_id="agent2", description="Step 2"),
            Task(agent_id="agent3", description="Step 3"),
        ]

        result = await patterns.pipeline(steps, "session1", "user1")

        assert not result.success
        assert "Step 2 failed" in result.output
        assert call_count == 2  # Should stop after failure

    @pytest.mark.asyncio
    async def test_adversarial_review_approval(self):
        """Test adversarial review with approval."""
        from unittest.mock import AsyncMock, MagicMock

        from core.agent.coordination import CoordinationPatterns
        from core.skills.delegation import DelegateTaskOutput

        # Mock delegation skill that approves on first review
        async def mock_execute(input_data):
            if "Review" in input_data.task:
                return DelegateTaskOutput(
                    success=True,
                    result="LGTM - Approved",
                    agent_id=input_data.agent_id,
                    events_produced=1,
                )
            return DelegateTaskOutput(
                success=True,
                result="Revised proposal",
                agent_id=input_data.agent_id,
                events_produced=1,
            )

        mock_delegate = MagicMock()
        mock_delegate.execute = AsyncMock(side_effect=mock_execute)

        patterns = CoordinationPatterns(mock_delegate)

        result = await patterns.adversarial_review(
            proposal="Initial proposal",
            proposer_agent="proposer",
            reviewer_agent="reviewer",
            session_id="session1",
            user_id="user1",
            max_rounds=3,
        )

        assert result.success
        assert result.output == "Initial proposal"  # Original approved
