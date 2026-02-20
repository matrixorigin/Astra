"""Test that planning mode records feedback for learning."""

import pytest
from unittest.mock import AsyncMock, MagicMock, patch
from core.agent.chat_loop import ChatLoop
from core.skills.learning_signals import SignalType


@pytest.mark.asyncio
async def test_planning_mode_uses_skill_pipeline():
    """Verify planning mode calls SkillPipeline.get_tools_schema()."""
    
    # Mock dependencies
    mock_llm = MagicMock()
    mock_db = MagicMock()
    mock_executor = MagicMock()
    mock_pipeline = MagicMock()
    mock_event_logger = MagicMock()
    mock_context_manager = MagicMock()
    mock_firewall = MagicMock()
    
    # Mock pipeline.get_tools_schema() return value
    mock_selection = MagicMock()
    mock_selection.tools = [{"function": {"name": "test_skill"}}]
    mock_selection.event_id = "sel_123"
    mock_pipeline.get_tools_schema.return_value = mock_selection
    
    # Mock executor.execute_skill()
    mock_executor.execute_skill_with_feedback.return_value = "test result"
    mock_executor.skill_registry = MagicMock()
    
    # Mock planner
    mock_plan = MagicMock()
    mock_step = MagicMock()
    mock_step.step_id = "step_1"
    mock_step.skill_hint = "test_skill"
    mock_step.description = "test description"
    mock_plan.steps = [mock_step]
    
    with patch("core.agent.chat_loop.Planner") as MockPlanner:
        mock_planner_instance = MagicMock()
        mock_planner_instance.create_plan = AsyncMock(return_value=mock_plan)
        mock_planner_instance.check_constraints.return_value = (True, None)
        mock_planner_instance.get_next_steps.side_effect = [[mock_step], []]  # First call returns step, second returns empty
        mock_planner_instance.constraints = MagicMock(max_steps=10)
        MockPlanner.return_value = mock_planner_instance
        
        # Create ChatLoop
        chat_loop = ChatLoop(
            selector=mock_pipeline,
            executor=mock_executor,
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=mock_context_manager,
            firewall=mock_firewall,
        )
        
        # Run planning
        events = []
        async for event in chat_loop.run_step_with_planning(
            user_input="test query",
            session_id="sess_1",
            user_id="user_1",
            max_candidates=5,
        ):
            events.append(event)
        
        # Verify pipeline.get_tools_schema() was called
        mock_pipeline.get_tools_schema.assert_called_once_with(
            query="test description",
            session_id="sess_1",
            max_candidates=5,
        )
        
        # Verify executor.execute_skill_with_feedback() was called (new encapsulated method)
        mock_executor.execute_skill_with_feedback.assert_called_once()


@pytest.mark.asyncio
async def test_planning_mode_records_feedback():
    """Verify planning mode records execution time feedback."""
    
    # Mock dependencies
    mock_llm = MagicMock()
    mock_db = MagicMock()
    mock_executor = MagicMock()
    mock_pipeline = MagicMock()
    mock_event_logger = MagicMock()
    mock_context_manager = MagicMock()
    mock_firewall = MagicMock()
    
    # Mock pipeline.get_tools_schema() return value
    mock_selection = MagicMock()
    mock_selection.tools = [{"function": {"name": "test_skill"}}]
    mock_selection.event_id = "sel_123"
    mock_pipeline.get_tools_schema.return_value = mock_selection
    
    # Mock executor.execute_skill_with_feedback()
    mock_executor.execute_skill_with_feedback.return_value = "test result"
    mock_executor.skill_registry = MagicMock()
    
    # Mock planner
    mock_plan = MagicMock()
    mock_step = MagicMock()
    mock_step.step_id = "step_1"
    mock_step.skill_hint = "test_skill"
    mock_step.description = "test description"
    mock_plan.steps = [mock_step]
    
    with patch("core.agent.chat_loop.Planner") as MockPlanner:
        mock_planner_instance = MagicMock()
        mock_planner_instance.create_plan = AsyncMock(return_value=mock_plan)
        mock_planner_instance.check_constraints.return_value = (True, None)
        mock_planner_instance.get_next_steps.side_effect = [[mock_step], []]
        mock_planner_instance.constraints = MagicMock(max_steps=10)
        MockPlanner.return_value = mock_planner_instance
        
        # Create ChatLoop
        chat_loop = ChatLoop(
            selector=mock_pipeline,
            executor=mock_executor,
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=mock_context_manager,
            firewall=mock_firewall,
        )
        
        # Run planning
        events = []
        async for event in chat_loop.run_step_with_planning(
            user_input="test query",
            session_id="sess_1",
            user_id="user_1",
            max_candidates=5,
        ):
            events.append(event)
        
        # Verify execute_skill_with_feedback was called with correct parameters
        mock_executor.execute_skill_with_feedback.assert_called_once()
        call_args = mock_executor.execute_skill_with_feedback.call_args
        
        # Check that selection_event_id and extra_feedback_data were passed
        assert call_args[1]["selection_event_id"] == "sel_123"
        assert call_args[1]["extra_feedback_data"]["planning_step"] == "step_1"


@pytest.mark.asyncio
async def test_planning_mode_skill_not_found():
    """Test when skill_hint is not in available tools."""
    
    mock_llm = MagicMock()
    mock_executor = MagicMock()
    mock_pipeline = MagicMock()
    mock_event_logger = MagicMock()
    mock_context_manager = MagicMock()
    mock_firewall = MagicMock()
    
    # Mock pipeline returns tools but NOT the requested skill
    mock_selection = MagicMock()
    mock_selection.tools = [{"function": {"name": "other_skill"}}]
    mock_selection.event_id = "sel_123"
    mock_pipeline.get_tools_schema.return_value = mock_selection
    
    mock_executor.skill_registry = MagicMock()
    
    # Mock planner
    mock_plan = MagicMock()
    mock_step = MagicMock()
    mock_step.step_id = "step_1"
    mock_step.skill_hint = "missing_skill"
    mock_step.description = "test description"
    mock_plan.steps = [mock_step]
    
    with patch("core.agent.chat_loop.Planner") as MockPlanner:
        mock_planner_instance = MagicMock()
        mock_planner_instance.create_plan = AsyncMock(return_value=mock_plan)
        mock_planner_instance.check_constraints.return_value = (True, None)
        mock_planner_instance.get_next_steps.side_effect = [[mock_step], []]
        mock_planner_instance.constraints = MagicMock(max_steps=10)
        MockPlanner.return_value = mock_planner_instance
        
        chat_loop = ChatLoop(
            selector=mock_pipeline,
            executor=mock_executor,
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=mock_context_manager,
            firewall=mock_firewall,
        )
        
        events = []
        async for event in chat_loop.run_step_with_planning(
            user_input="test query",
            session_id="sess_1",
            user_id="user_1",
            max_candidates=5,
        ):
            events.append(event)
        
        # Verify executor.execute_skill() was NOT called
        mock_executor.execute_skill_with_feedback.assert_not_called()
        
        # Verify feedback was NOT recorded (no execution happened)
        mock_pipeline.record_feedback.assert_not_called()
        
        # Verify step completed with error message
        assert mock_step.result == "Skill missing_skill not available"


@pytest.mark.asyncio
async def test_planning_mode_no_skill_hint():
    """Test when step has no skill_hint (plain chat execution)."""
    
    mock_llm = MagicMock()
    mock_executor = MagicMock()
    mock_pipeline = MagicMock()
    mock_event_logger = MagicMock()
    mock_context_manager = MagicMock()
    mock_firewall = MagicMock()
    
    mock_executor.skill_registry = MagicMock()
    
    # Mock planner
    mock_plan = MagicMock()
    mock_step = MagicMock()
    mock_step.step_id = "step_1"
    mock_step.skill_hint = None  # No skill hint
    mock_step.description = "test description"
    mock_plan.steps = [mock_step]
    
    with patch("core.agent.chat_loop.Planner") as MockPlanner:
        mock_planner_instance = MagicMock()
        mock_planner_instance.create_plan = AsyncMock(return_value=mock_plan)
        mock_planner_instance.check_constraints.return_value = (True, None)
        mock_planner_instance.get_next_steps.side_effect = [[mock_step], []]
        mock_planner_instance.constraints = MagicMock(max_steps=10)
        MockPlanner.return_value = mock_planner_instance
        
        chat_loop = ChatLoop(
            selector=mock_pipeline,
            executor=mock_executor,
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=mock_context_manager,
            firewall=mock_firewall,
        )
        
        events = []
        async for event in chat_loop.run_step_with_planning(
            user_input="test query",
            session_id="sess_1",
            user_id="user_1",
            max_candidates=5,
        ):
            events.append(event)
        
        # Verify pipeline was NOT called (no skill to select)
        mock_pipeline.get_tools_schema.assert_not_called()
        
        # Verify executor was NOT called
        mock_executor.execute_skill_with_feedback.assert_not_called()
        
        # Verify feedback was NOT recorded
        mock_pipeline.record_feedback.assert_not_called()
        
        # Verify step completed with default message
        assert mock_step.result == "Step executed"


@pytest.mark.asyncio
async def test_planning_mode_multiple_steps():
    """Test planning with multiple steps executes all and records feedback for each."""
    
    mock_llm = MagicMock()
    mock_executor = MagicMock()
    mock_pipeline = MagicMock()
    mock_event_logger = MagicMock()
    mock_context_manager = MagicMock()
    mock_firewall = MagicMock()
    
    # Mock pipeline returns different tools for each step
    mock_selection_1 = MagicMock()
    mock_selection_1.tools = [{"function": {"name": "skill_1"}}]
    mock_selection_1.event_id = "sel_1"
    
    mock_selection_2 = MagicMock()
    mock_selection_2.tools = [{"function": {"name": "skill_2"}}]
    mock_selection_2.event_id = "sel_2"
    
    mock_pipeline.get_tools_schema.side_effect = [mock_selection_1, mock_selection_2]
    
    # Mock executor returns different results
    mock_executor.execute_skill_with_feedback.side_effect = ["result_1", "result_2"]
    mock_executor.skill_registry = MagicMock()
    
    # Mock planner with 2 steps
    mock_plan = MagicMock()
    mock_step_1 = MagicMock()
    mock_step_1.step_id = "step_1"
    mock_step_1.skill_hint = "skill_1"
    mock_step_1.description = "description_1"
    
    mock_step_2 = MagicMock()
    mock_step_2.step_id = "step_2"
    mock_step_2.skill_hint = "skill_2"
    mock_step_2.description = "description_2"
    
    mock_plan.steps = [mock_step_1, mock_step_2]
    
    with patch("core.agent.chat_loop.Planner") as MockPlanner:
        mock_planner_instance = MagicMock()
        mock_planner_instance.create_plan = AsyncMock(return_value=mock_plan)
        mock_planner_instance.check_constraints.return_value = (True, None)
        mock_planner_instance.get_next_steps.side_effect = [
            [mock_step_1, mock_step_2],  # First call returns both steps
            []  # Second call returns empty (all done)
        ]
        mock_planner_instance.constraints = MagicMock(max_steps=10)
        MockPlanner.return_value = mock_planner_instance
        
        chat_loop = ChatLoop(
            selector=mock_pipeline,
            executor=mock_executor,
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=mock_context_manager,
            firewall=mock_firewall,
        )
        
        events = []
        async for event in chat_loop.run_step_with_planning(
            user_input="test query",
            session_id="sess_1",
            user_id="user_1",
            max_candidates=5,
        ):
            events.append(event)
        
        # Verify pipeline was called twice
        assert mock_pipeline.get_tools_schema.call_count == 2
        
        # Verify executor was called twice with feedback
        assert mock_executor.execute_skill_with_feedback.call_count == 2
        
        # Verify feedback parameters were passed correctly
        call_1 = mock_executor.execute_skill_with_feedback.call_args_list[0]
        assert call_1[1]["selection_event_id"] == "sel_1"
        assert call_1[1]["extra_feedback_data"]["planning_step"] == "step_1"
        
        call_2 = mock_executor.execute_skill_with_feedback.call_args_list[1]
        assert call_2[1]["selection_event_id"] == "sel_2"
        assert call_2[1]["extra_feedback_data"]["planning_step"] == "step_2"


@pytest.mark.asyncio
async def test_planning_mode_execution_error_propagates():
    """Test that execution errors propagate up (not silently caught)."""
    
    mock_llm = MagicMock()
    mock_executor = MagicMock()
    mock_pipeline = MagicMock()
    mock_event_logger = MagicMock()
    mock_context_manager = MagicMock()
    mock_firewall = MagicMock()
    
    mock_selection = MagicMock()
    mock_selection.tools = [{"function": {"name": "test_skill"}}]
    mock_selection.event_id = "sel_123"
    mock_pipeline.get_tools_schema.return_value = mock_selection
    
    # Mock executor raises exception
    mock_executor.execute_skill_with_feedback.side_effect = RuntimeError("Execution failed")
    mock_executor.skill_registry = MagicMock()
    
    mock_plan = MagicMock()
    mock_step = MagicMock()
    mock_step.step_id = "step_1"
    mock_step.skill_hint = "test_skill"
    mock_step.description = "test description"
    mock_plan.steps = [mock_step]
    
    with patch("core.agent.chat_loop.Planner") as MockPlanner:
        mock_planner_instance = MagicMock()
        mock_planner_instance.create_plan = AsyncMock(return_value=mock_plan)
        mock_planner_instance.check_constraints.return_value = (True, None)
        mock_planner_instance.get_next_steps.side_effect = [[mock_step], []]
        mock_planner_instance.constraints = MagicMock(max_steps=10)
        MockPlanner.return_value = mock_planner_instance
        
        chat_loop = ChatLoop(
            selector=mock_pipeline,
            executor=mock_executor,
            llm_client=mock_llm,
            event_logger=mock_event_logger,
            context_manager=mock_context_manager,
            firewall=mock_firewall,
        )
        
        # Exception should propagate (not be caught)
        with pytest.raises(RuntimeError, match="Execution failed"):
            async for event in chat_loop.run_step_with_planning(
                user_input="test query",
                session_id="sess_1",
                user_id="user_1",
                max_candidates=5,
            ):
                pass
        
        # Verify execute_skill_with_feedback was called
        # (feedback is recorded in its try-finally before exception propagates)
        mock_executor.execute_skill_with_feedback.assert_called_once()


@pytest.mark.asyncio
async def test_execute_skill_with_feedback_no_pipeline():
    """Test execute_skill_with_feedback works when pipeline is None (backward compatibility)."""
    from core.agent.executor import AgentExecutor
    from core.skills.mocking import MockMode
    from unittest.mock import MagicMock, patch
    
    mock_db = MagicMock()
    mock_registry = MagicMock()
    mock_skill = MagicMock()
    mock_skill.validate_input.return_value = {}
    mock_registry.get.return_value = mock_skill
    
    # Mock ToolMockingLayer to avoid DB validation
    with patch('core.agent.executor.ToolMockingLayer') as MockToolMockingLayer:
        mock_mocking_layer = MagicMock()
        mock_mocking_layer.execute.return_value = "test result"
        MockToolMockingLayer.return_value = mock_mocking_layer
        
        # Create executor WITHOUT pipeline
        executor = AgentExecutor(
            db=mock_db,
            registry=mock_registry,
            mode=MockMode.PRODUCTION,
            pipeline=None,  # No pipeline
        )
        
        # Should not raise error even without pipeline
        result = executor.execute_skill_with_feedback(
            skill_name="test_skill",
            params={},
            session_id="sess_1",
            parent_event_id=None,
            selection_event_id="sel_123",  # Even with selection_event_id
        )
        
        assert result == "test result"
        # Verify execute was called
        mock_mocking_layer.execute.assert_called_once()
