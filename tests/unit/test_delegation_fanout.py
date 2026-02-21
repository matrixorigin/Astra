"""Tests for multi-agent delegation with fan-out/fan-in."""

import asyncio
import pytest
from unittest.mock import AsyncMock, MagicMock, patch

from core.skills.delegation import DelegateTaskSkill, DelegateTaskInput
from core.events.models import StreamEvent, StreamEventType


@pytest.fixture(autouse=True)
def clean_fan_in_tasks():
    """Clean fan-in tasks after each test."""
    from core.agent.run_engine import _fan_in_tasks
    yield
    # Cancel any pending fan-in tasks
    for task in _fan_in_tasks:
        task.cancel()
    _fan_in_tasks.clear()


@pytest.fixture
def mock_agent_registry():
    """Mock agent registry."""
    registry = MagicMock()
    
    # Mock agent profiles
    code_agent = MagicMock()
    code_agent.system_prompt = "You are a code review agent"
    
    security_agent = MagicMock()
    security_agent.system_prompt = "You are a security review agent"
    
    test_agent = MagicMock()
    test_agent.system_prompt = "You are a test agent"
    
    registry.get = lambda agent_id: {
        "code_agent": code_agent,
        "security_agent": security_agent,
        "test_agent": test_agent,
    }.get(agent_id)
    
    return registry


@pytest.fixture
def mock_chat_loop_factory():
    """Mock chat loop factory."""
    async def mock_stream(*args, **kwargs):
        """Mock stream that yields events."""
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
        
        yield StreamEvent(
            event_type=StreamEventType.RUN_STARTED,
            data={},
            agent_id=agent_id,
        )
        
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DELTA,
            data={"text": f"Result from {agent_id}"},
            agent_id=agent_id,
        )
        
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"text": f"Result from {agent_id}"},
            agent_id=agent_id,
        )
        
        yield StreamEvent(
            event_type=StreamEventType.RUN_FINISHED,
            data={},
            agent_id=agent_id,
        )
    
    async def mock_run_step(*args, **kwargs):
        """Mock non-streaming run_step."""
        agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
        return f"Result from {agent_id}"
    
    def factory(system_prompt, agent_id):
        loop = MagicMock()
        loop.run_step_stream = mock_stream
        loop.run_step = mock_run_step
        return loop
    
    return factory


class TestStreamMultiplexing:
    """Test stream multiplexing from delegated agents."""
    
    @pytest.mark.asyncio
    async def test_stream_forwarding_with_agent_id(self, mock_agent_registry, mock_chat_loop_factory):
        """Test that delegated agent's events are forwarded with agent_id tagged."""
        skill = DelegateTaskSkill(mock_agent_registry, mock_chat_loop_factory)
        
        input_data = DelegateTaskInput(
            agent_id="code_agent",
            task="Review this code",
            session_id="test_session",
            user_id="test_user",
        )
        
        events = []
        async for event in skill.execute_stream(input_data):
            events.append(event)
        
        # Check delegation markers
        assert events[0].event_type == StreamEventType.AGENT_DELEGATED
        assert events[0].agent_id == "code_agent"
        assert events[-1].event_type == StreamEventType.AGENT_COMPLETED
        assert events[-1].agent_id == "code_agent"
        
        # Check forwarded events have agent_id
        for event in events[1:-1]:
            assert event.agent_id == "code_agent"
    
    @pytest.mark.asyncio
    async def test_stream_error_handling(self, mock_agent_registry, mock_chat_loop_factory):
        """Test error handling in streaming delegation."""
        skill = DelegateTaskSkill(mock_agent_registry, mock_chat_loop_factory)
        
        # Non-existent agent
        input_data = DelegateTaskInput(
            agent_id="nonexistent_agent",
            task="Do something",
            session_id="test_session",
            user_id="test_user",
        )
        
        events = []
        async for event in skill.execute_stream(input_data):
            events.append(event)
        
        assert len(events) == 1
        assert events[0].event_type == StreamEventType.RUN_ERROR
        assert "not found" in events[0].data["error"]


class TestFanOutFanIn:
    """Test parallel delegation with fan-out/fan-in."""
    
    @pytest.mark.asyncio
    async def test_parallel_execution(self, mock_agent_registry, mock_chat_loop_factory):
        """Test parallel execution of multiple delegations."""
        skill = DelegateTaskSkill(mock_agent_registry, mock_chat_loop_factory)
        
        inputs = [
            DelegateTaskInput(
                agent_id="code_agent",
                task="Review code",
                session_id="test_session",
                user_id="test_user",
            ),
            DelegateTaskInput(
                agent_id="security_agent",
                task="Review security",
                session_id="test_session",
                user_id="test_user",
            ),
            DelegateTaskInput(
                agent_id="test_agent",
                task="Review tests",
                session_id="test_session",
                user_id="test_user",
            ),
        ]
        
        results = await skill.execute_parallel(inputs)
        
        assert len(results) == 3
        assert all(r.success for r in results)
        assert results[0].agent_id == "code_agent"
        assert results[1].agent_id == "security_agent"
        assert results[2].agent_id == "test_agent"
    
    @pytest.mark.asyncio
    async def test_parallel_streaming(self, mock_agent_registry, mock_chat_loop_factory):
        """Test parallel streaming with multiplexed events."""
        skill = DelegateTaskSkill(mock_agent_registry, mock_chat_loop_factory)
        
        inputs = [
            DelegateTaskInput(
                agent_id="code_agent",
                task="Review code",
                session_id="test_session",
                user_id="test_user",
            ),
            DelegateTaskInput(
                agent_id="security_agent",
                task="Review security",
                session_id="test_session",
                user_id="test_user",
            ),
        ]
        
        events = []
        async for event in skill.execute_parallel_stream(inputs):
            events.append(event)
        
        # Check we got events from both agents
        agent_ids = {e.agent_id for e in events if e.agent_id}
        assert "code_agent" in agent_ids
        assert "security_agent" in agent_ids
        
        # Check final aggregation
        aggregation_events = [e for e in events if e.event_type == StreamEventType.AGENT_PROGRESS]
        assert len(aggregation_events) == 1
        
        agg_data = aggregation_events[0].data["aggregated_results"]
        assert agg_data["total"] == 2
        assert agg_data["successful"] >= 0
        assert len(agg_data["delegations"]) == 2
    
    @pytest.mark.asyncio
    async def test_parallel_with_failures(self, mock_agent_registry, mock_chat_loop_factory):
        """Test parallel execution with some failures."""
        skill = DelegateTaskSkill(mock_agent_registry, mock_chat_loop_factory)
        
        inputs = [
            DelegateTaskInput(
                agent_id="code_agent",
                task="Review code",
                session_id="test_session",
                user_id="test_user",
            ),
            DelegateTaskInput(
                agent_id="nonexistent_agent",
                task="Do something",
                session_id="test_session",
                user_id="test_user",
            ),
        ]
        
        results = await skill.execute_parallel(inputs)
        
        assert len(results) == 2
        assert results[0].success is True
        assert results[1].success is False
        assert "not found" in results[1].result


class TestResultAggregation:
    """Test result aggregation from multiple agents."""
    
    @pytest.mark.asyncio
    async def test_aggregation_structure(self, mock_agent_registry, mock_chat_loop_factory):
        """Test aggregated results have correct structure."""
        skill = DelegateTaskSkill(mock_agent_registry, mock_chat_loop_factory)
        
        inputs = [
            DelegateTaskInput(
                agent_id="code_agent",
                task="Task 1",
                session_id="test_session",
                user_id="test_user",
            ),
            DelegateTaskInput(
                agent_id="security_agent",
                task="Task 2",
                session_id="test_session",
                user_id="test_user",
            ),
        ]
        
        events = []
        async for event in skill.execute_parallel_stream(inputs):
            events.append(event)
        
        # Find aggregation event
        agg_events = [e for e in events if e.event_type == StreamEventType.AGENT_PROGRESS]
        assert len(agg_events) == 1
        
        agg = agg_events[0].data["aggregated_results"]
        
        # Check structure
        assert "delegations" in agg
        assert "total" in agg
        assert "successful" in agg
        assert "failed" in agg
        
        # Check delegation details
        for delegation in agg["delegations"]:
            assert "agent_id" in delegation
            assert "task" in delegation
            assert "result" in delegation
            assert "success" in delegation


class TestErrorScenarios:
    """Test error handling in delegation."""
    
    @pytest.mark.asyncio
    async def test_stream_exception_handling(self, mock_agent_registry):
        """Test handling of exceptions during streaming."""
        
        async def failing_stream(*args, **kwargs):
            """Stream that raises exception."""
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={},
                agent_id="failing_agent",
            )
            raise RuntimeError("Stream failed")
        
        def factory(system_prompt, agent_id):
            loop = MagicMock()
            loop.run_step_stream = failing_stream
            return loop
        
        skill = DelegateTaskSkill(mock_agent_registry, factory)
        
        input_data = DelegateTaskInput(
            agent_id="code_agent",
            task="Do something",
            session_id="test_session",
            user_id="test_user",
        )
        
        events = []
        async for event in skill.execute_stream(input_data):
            events.append(event)
        
        # Should have error event
        error_events = [e for e in events if e.event_type == StreamEventType.RUN_ERROR]
        assert len(error_events) > 0
        assert "failed" in error_events[0].data["error"].lower()
    
    @pytest.mark.asyncio
    async def test_parallel_with_stream_exceptions(self, mock_agent_registry):
        """Test parallel execution when some streams fail."""
        
        async def mixed_stream(*args, **kwargs):
            """Stream that succeeds or fails based on agent_id."""
            agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
            
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={},
                agent_id=agent_id,
            )
            
            if agent_id == "failing_agent":
                raise RuntimeError("Intentional failure")
            
            yield StreamEvent(
                event_type=StreamEventType.TEXT_DONE,
                data={"text": f"Success from {agent_id}"},
                agent_id=agent_id,
            )
            
            yield StreamEvent(
                event_type=StreamEventType.RUN_FINISHED,
                data={},
                agent_id=agent_id,
            )
        
        def factory(system_prompt, agent_id):
            loop = MagicMock()
            loop.run_step_stream = mixed_stream
            return loop
        
        # Add failing agent to registry
        failing_agent = MagicMock()
        failing_agent.system_prompt = "Failing agent"
        original_get = mock_agent_registry.get
        
        def get_with_failing(agent_id):
            if agent_id == "failing_agent":
                return failing_agent
            return original_get(agent_id)
        
        mock_agent_registry.get = get_with_failing
        
        skill = DelegateTaskSkill(mock_agent_registry, factory)
        
        inputs = [
            DelegateTaskInput(
                agent_id="code_agent",
                task="Task 1",
                session_id="test_session",
                user_id="test_user",
            ),
            DelegateTaskInput(
                agent_id="failing_agent",
                task="Task 2",
                session_id="test_session",
                user_id="test_user",
            ),
        ]
        
        events = []
        async for event in skill.execute_parallel_stream(inputs):
            events.append(event)
        
        # Check aggregation shows mixed results
        agg_events = [e for e in events if e.event_type == StreamEventType.AGENT_PROGRESS]
        assert len(agg_events) == 1
        
        agg = agg_events[0].data["aggregated_results"]
        assert agg["total"] == 2
        assert agg["successful"] == 1  # Only code_agent succeeded
        assert agg["failed"] == 1  # failing_agent failed
        
        # Check error was captured
        error_events = [e for e in events if e.event_type == StreamEventType.RUN_ERROR]
        assert len(error_events) > 0
    
    @pytest.mark.asyncio
    async def test_no_text_done_event(self, mock_agent_registry):
        """Test handling when stream completes without TEXT_DONE event."""
        
        async def incomplete_stream(*args, **kwargs):
            """Stream that completes without TEXT_DONE."""
            agent_id = kwargs.get("context", {}).get("agent_id", "unknown")
            
            yield StreamEvent(
                event_type=StreamEventType.RUN_STARTED,
                data={},
                agent_id=agent_id,
            )
            
            # No TEXT_DONE event
            
            yield StreamEvent(
                event_type=StreamEventType.AGENT_COMPLETED,
                data={},
                agent_id=agent_id,
            )
        
        def factory(system_prompt, agent_id):
            loop = MagicMock()
            loop.run_step_stream = incomplete_stream
            return loop
        
        skill = DelegateTaskSkill(mock_agent_registry, factory)
        
        inputs = [
            DelegateTaskInput(
                agent_id="code_agent",
                task="Task",
                session_id="test_session",
                user_id="test_user",
            ),
        ]
        
        events = []
        async for event in skill.execute_parallel_stream(inputs):
            events.append(event)
        
        # Check aggregation handles missing result
        agg_events = [e for e in events if e.event_type == StreamEventType.AGENT_PROGRESS]
        assert len(agg_events) == 1
        
        agg = agg_events[0].data["aggregated_results"]
        assert agg["total"] == 1
        # Should still count as successful (completed without error)
        assert agg["successful"] == 1
        assert agg["delegations"][0]["result"] == ""  # Empty but valid
