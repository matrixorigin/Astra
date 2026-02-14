"""Integration tests for multi-agent delegation workflows."""

import pytest

from core.agent.agent_registry import AgentProfile, AgentRegistry
from core.agent.coordination import CoordinationPatterns, Task
from core.skills.delegation import DelegateTaskInput, DelegateTaskOutput, DelegateTaskSkill


class TestMultiAgentWorkflows:
    """Test end-to-end multi-agent workflows."""

    @pytest.mark.asyncio
    async def test_orchestrator_fan_out_workflow(self):
        """Test orchestrator delegating to multiple agents in parallel."""
        from unittest.mock import AsyncMock, MagicMock

        # Setup agent registry
        registry = AgentRegistry()
        registry.register(AgentProfile(agent_id="code_agent", system_prompt="Code expert"))
        registry.register(AgentProfile(agent_id="security_agent", system_prompt="Security expert"))
        registry.register(AgentProfile(agent_id="perf_agent", system_prompt="Performance expert"))
        registry.register(
            AgentProfile(
                agent_id="orchestrator",
                system_prompt="Orchestrate reviews",
                tier="orchestrator",
                can_delegate=True,
                delegate_to=["code_agent", "security_agent", "perf_agent"],
            )
        )

        # Mock chat loop factory
        async def mock_run_step(user_input, session_id, user_id, context=None):
            agent_id = context.get("agent_id", "unknown")
            return f"[{agent_id}] Review complete: {user_input[:50]}"

        mock_loop = MagicMock()
        mock_loop.run_step = AsyncMock(side_effect=mock_run_step)

        def loop_factory(system_prompt=None, agent_id="dev-agent"):
            return mock_loop

        # Create delegation skill
        delegate_skill = DelegateTaskSkill(registry, loop_factory)

        # Create coordination patterns
        patterns = CoordinationPatterns(delegate_skill)

        # Execute fan-out workflow
        tasks = [
            Task(agent_id="code_agent", description="Review code quality"),
            Task(agent_id="security_agent", description="Review security"),
            Task(agent_id="perf_agent", description="Review performance"),
        ]

        results = await patterns.fan_out(tasks, "session1", "user1")

        # Verify all tasks completed
        assert len(results) == 3
        assert all(r.success for r in results)
        assert "code_agent" in results[0].output
        assert "security_agent" in results[1].output
        assert "perf_agent" in results[2].output

        # Verify parallel execution (all called)
        assert mock_loop.run_step.call_count == 3

    @pytest.mark.asyncio
    async def test_pipeline_workflow(self):
        """Test sequential pipeline with output passing."""
        from unittest.mock import AsyncMock, MagicMock

        # Setup agent registry
        registry = AgentRegistry()
        registry.register(AgentProfile(agent_id="analyzer", system_prompt="Analyze code"))
        registry.register(AgentProfile(agent_id="fixer", system_prompt="Fix issues"))
        registry.register(AgentProfile(agent_id="tester", system_prompt="Test fixes"))

        # Mock chat loop with sequential outputs
        call_count = 0
        outputs = [
            "Found 3 issues: bug1, bug2, bug3",
            "Fixed all 3 issues",
            "All tests pass",
        ]

        async def mock_run_step(user_input, session_id, user_id, context=None):
            nonlocal call_count
            result = outputs[call_count]
            call_count += 1
            return result

        mock_loop = MagicMock()
        mock_loop.run_step = AsyncMock(side_effect=mock_run_step)

        def loop_factory(system_prompt=None, agent_id="dev-agent"):
            return mock_loop

        # Create delegation skill and patterns
        delegate_skill = DelegateTaskSkill(registry, loop_factory)
        patterns = CoordinationPatterns(delegate_skill)

        # Execute pipeline
        steps = [
            Task(agent_id="analyzer", description="Analyze code"),
            Task(agent_id="fixer", description="Fix issues"),
            Task(agent_id="tester", description="Test fixes"),
        ]

        result = await patterns.pipeline(steps, "session1", "user1")

        # Verify pipeline completed
        assert result.success
        assert result.output == "All tests pass"
        assert call_count == 3

    @pytest.mark.asyncio
    async def test_adversarial_review_workflow(self):
        """Test adversarial review with revision loop."""
        from unittest.mock import AsyncMock, MagicMock

        # Setup agent registry
        registry = AgentRegistry()
        registry.register(AgentProfile(agent_id="proposer", system_prompt="Propose solutions"))
        registry.register(AgentProfile(agent_id="reviewer", system_prompt="Review proposals"))

        # Mock chat loop with review/revision cycle
        call_count = 0

        async def mock_run_step(user_input, session_id, user_id, context=None):
            nonlocal call_count
            call_count += 1

            if "Review" in user_input:
                # Reviewer responses
                if call_count == 1:
                    return "Needs improvement: add error handling"
                elif call_count == 3:
                    return "LGTM - Approved"
            else:
                # Proposer responses
                return "Revised proposal with error handling"

        mock_loop = MagicMock()
        mock_loop.run_step = AsyncMock(side_effect=mock_run_step)

        def loop_factory(system_prompt=None, agent_id="dev-agent"):
            return mock_loop

        # Create delegation skill and patterns
        delegate_skill = DelegateTaskSkill(registry, loop_factory)
        patterns = CoordinationPatterns(delegate_skill)

        # Execute adversarial review
        result = await patterns.adversarial_review(
            proposal="Initial proposal",
            proposer_agent="proposer",
            reviewer_agent="reviewer",
            session_id="session1",
            user_id="user1",
            max_rounds=3,
        )

        # Verify approval after revision
        assert result.success
        assert "error handling" in result.output
        assert call_count >= 2  # At least one review and one revision

    @pytest.mark.asyncio
    async def test_delegation_permission_enforcement(self):
        """Test that delegation permissions are enforced."""
        from unittest.mock import MagicMock

        # Setup agent registry with restricted delegation
        registry = AgentRegistry()
        registry.register(AgentProfile(agent_id="allowed_agent", system_prompt="Allowed"))
        registry.register(AgentProfile(agent_id="forbidden_agent", system_prompt="Forbidden"))
        registry.register(
            AgentProfile(
                agent_id="restricted_orchestrator",
                system_prompt="Restricted orchestrator",
                tier="orchestrator",
                can_delegate=True,
                delegate_to=["allowed_agent"],  # Only allowed_agent
            )
        )

        # Verify permission checks
        assert registry.can_delegate("restricted_orchestrator", "allowed_agent")
        assert not registry.can_delegate("restricted_orchestrator", "forbidden_agent")

    @pytest.mark.asyncio
    async def test_event_chain_propagation(self):
        """Test that causal chain propagates through delegation."""
        from unittest.mock import AsyncMock, MagicMock

        # Setup agent registry
        registry = AgentRegistry()
        registry.register(AgentProfile(agent_id="worker", system_prompt="Worker agent"))

        # Mock chat loop that captures context
        captured_context = {}

        async def mock_run_step(user_input, session_id, user_id, context=None):
            captured_context.update(context or {})
            return "Task complete"

        mock_loop = MagicMock()
        mock_loop.run_step = AsyncMock(side_effect=mock_run_step)

        def loop_factory(system_prompt=None, agent_id="dev-agent"):
            return mock_loop

        # Create delegation skill
        delegate_skill = DelegateTaskSkill(registry, loop_factory)

        # Execute delegation
        input_data = DelegateTaskInput(
            agent_id="worker",
            task="Do work",
            context="parent_context",
            session_id="session1",
            user_id="user1",
        )

        result = await delegate_skill.execute(input_data)

        # Verify context propagation
        assert result.success
        assert captured_context.get("agent_id") == "worker"
        assert captured_context.get("delegation_context") == "parent_context"
