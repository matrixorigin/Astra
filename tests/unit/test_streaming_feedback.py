"""Test to verify feedback recording in streaming execution path."""

import asyncio
import pytest
from unittest.mock import Mock, AsyncMock, patch
from core.agent.chat_loop import ChatLoop
from core.skills.learning_signals import SignalType


@pytest.mark.asyncio
@patch('core.agent.chat_loop._needs_planning', new_callable=AsyncMock)
async def test_streaming_records_feedback(mock_needs_planning):
    """Verify that run_step_stream records feedback signals."""
    
    # Disable planning for this test
    mock_needs_planning.return_value = False
    
    # Mock dependencies
    mock_pipeline = Mock()
    mock_pipeline.get_tools_schema = Mock(return_value=Mock(
        tools=[{
            "type": "function",
            "function": {
                "name": "test_skill",
                "description": "Test skill",
                "parameters": {"type": "object", "properties": {}}
            }
        }],
        event_id="test_event_123",
        candidates=1
    ))
    mock_pipeline.record_feedback = Mock()
    
    mock_executor = Mock()
    mock_executor.execute_skill_with_feedback = Mock(return_value="test result")
    
    mock_llm = Mock()
    
    # Simulate LLM streaming: first tool call, then final text
    call_count = [0]
    async def mock_stream(*args, **kwargs):
        call_count[0] += 1
        if call_count[0] == 1:
            # First round: tool call
            yield {"type": "tool_call", "data": {
                "id": "call_1",
                "function": {"name": "test_skill", "arguments": "{}"}
            }}
        else:
            # Second round: final text (no tool calls)
            yield {"type": "text", "content": "Done"}
    
    mock_llm.chat_with_tools_stream = mock_stream
    
    mock_event_logger = Mock()
    mock_event_logger.create_user_query = Mock(return_value=Mock(
        event_id="user_event_1",
        causal_chain_id="chain_1"
    ))
    mock_event_logger.create_stream_event = Mock(return_value=Mock(
        event_id="stream_event_1",
        causal_chain_id="chain_1"
    ))
    
    mock_context_manager = Mock()
    mock_context_manager.build_context = Mock(return_value=Mock(
        system_prompt="test prompt",
        skill_definitions=[],
        selected_events=[],
        retrieved_events=[],
        code_context=[],
        documentation=[],
        total_tokens=100,
        token_budget={},
        assembly_time_ms=10,
        relevance_scores={},
        task_type="general"
    ))
    mock_context_manager.save_snapshot = Mock(return_value="snapshot_123")
    
    mock_firewall = Mock()
    mock_firewall.verify_response = Mock(return_value=Mock(
        safe_to_deliver=True,
        confidence_score=0.9,
        claims_verified=0,
        claims_failed=0
    ))
    mock_firewall.log_verification = Mock()
    
    # Create ChatLoop
    chat_loop = ChatLoop(
        selector=mock_pipeline,
        executor=mock_executor,
        llm_client=mock_llm,
        event_logger=mock_event_logger,
        context_manager=mock_context_manager,
        firewall=mock_firewall,
    )
    
    # Run streaming execution
    events = []
    async for event in chat_loop.run_step_stream(
        user_input="test query",
        session_id="session_1",
        user_id="user_1",
    ):
        events.append(event)
    
    # Verify execute_skill_with_feedback was called (feedback is handled internally)
    assert mock_executor.execute_skill_with_feedback.called, "execute_skill_with_feedback should be called"
    
    # Check the call arguments include selection_event_id
    call_args = mock_executor.execute_skill_with_feedback.call_args
    assert call_args[1]["selection_event_id"] == "test_event_123", "Should pass selection event_id for feedback"


@pytest.mark.asyncio
@patch('core.agent.chat_loop._needs_planning', new_callable=AsyncMock)
async def test_parallel_delegation_records_feedback(mock_needs_planning):
    """Verify that parallel delegation records feedback."""
    
    # Disable planning for this test
    mock_needs_planning.return_value = False
    
    # Mock dependencies
    mock_pipeline = Mock()
    mock_pipeline.get_tools_schema = Mock(return_value=Mock(
        tools=[{
            "type": "function",
            "function": {
                "name": "delegate_task",
                "description": "Delegate task",
                "parameters": {"type": "object", "properties": {}}
            }
        }],
        event_id="test_event_456",
        candidates=1
    ))
    mock_pipeline.record_feedback = Mock()
    
    mock_executor = Mock()
    mock_skill = Mock()
    
    # Simulate parallel execution completion
    async def mock_parallel_stream(*args, **kwargs):
        from core.events.models import StreamEvent, StreamEventType
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"text": "Agent 1 done"},
            agent_id="agent_1"
        )
        yield StreamEvent(
            event_type=StreamEventType.TEXT_DONE,
            data={"text": "Agent 2 done"},
            agent_id="agent_2"
        )
    
    mock_skill.execute_parallel_stream = mock_parallel_stream
    mock_executor.skill_registry = {"delegate_task": mock_skill}
    
    mock_llm = Mock()
    
    # Simulate LLM streaming: parallel delegation calls, then final text
    call_count = [0]
    async def mock_stream(*args, **kwargs):
        call_count[0] += 1
        if call_count[0] == 1:
            # First round: parallel delegation calls
            yield {"type": "tool_call", "data": {
                "id": "call_1",
                "function": {"name": "delegate_task", "arguments": '{"agent_id": "agent_1", "task": "task1"}'}
            }}
            yield {"type": "tool_call", "data": {
                "id": "call_2",
                "function": {"name": "delegate_task", "arguments": '{"agent_id": "agent_2", "task": "task2"}'}
            }}
        else:
            # Second round: final text
            yield {"type": "text", "content": "All done"}
    
    mock_llm.chat_with_tools_stream = mock_stream
    
    mock_event_logger = Mock()
    mock_event_logger.create_user_query = Mock(return_value=Mock(
        event_id="user_event_2",
        causal_chain_id="chain_2"
    ))
    mock_event_logger.create_stream_event = Mock(return_value=Mock(
        event_id="stream_event_2",
        causal_chain_id="chain_2"
    ))
    
    mock_context_manager = Mock()
    mock_firewall = Mock()
    
    # Create ChatLoop
    chat_loop = ChatLoop(
        selector=mock_pipeline,
        executor=mock_executor,
        llm_client=mock_llm,
        event_logger=mock_event_logger,
        context_manager=mock_context_manager,
        firewall=mock_firewall,
    )
    
    # Run streaming execution
    events = []
    async for event in chat_loop.run_step_stream(
        user_input="test parallel query",
        session_id="session_2",
        user_id="user_2",
    ):
        events.append(event)
    
    # Verify feedback was recorded for parallel execution
    assert mock_pipeline.record_feedback.called, "record_feedback should be called for parallel execution"
    
    # Check the call arguments
    calls = mock_pipeline.record_feedback.call_args_list
    
    # Find the parallel delegation feedback
    parallel_feedback = None
    for call in calls:
        event_id, signal_type, data = call[0]
        if data.get("parallel"):
            parallel_feedback = (event_id, signal_type, data)
            break
    
    assert parallel_feedback is not None, "Should have parallel delegation feedback"
    event_id, signal_type, data = parallel_feedback
    
    assert event_id == "test_event_456", "Should use selection event_id"
    assert signal_type == SignalType.EXECUTION_TIME, "Should record execution time"
    assert "ms" in data, "Should include execution time in ms"
    assert data["skill"] == "delegate_task", "Should be delegate_task"
    assert data["parallel"] is True, "Should mark as parallel"
    assert data["count"] == 2, "Should record number of parallel calls"


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
