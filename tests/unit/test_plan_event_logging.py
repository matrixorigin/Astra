"""Unit tests for plan event logging."""

from unittest.mock import MagicMock, patch

import pytest

from core.agent.planner import Plan, PlanStatus, PlanStep, Planner, restore_plan_from_events
from core.events.event_logger import EventLogger


@pytest.fixture
def mock_db():
    """Mock database."""
    db = MagicMock()
    db.fetchone.return_value = None
    db.fetchall.return_value = []
    return db


@pytest.fixture
def event_logger(mock_db):
    """EventLogger with mocked database."""
    return EventLogger(mock_db)


@pytest.fixture
def mock_llm():
    """Mock LLM client."""
    llm = MagicMock()
    llm.config = {"max_revisions": 3}
    return llm


@pytest.fixture
def planner(mock_llm, event_logger):
    """Planner with event logger."""
    return Planner(mock_llm, event_logger=event_logger)


class TestPlanEventLogging:
    """Tests for plan event logging."""

    def test_create_plan_event(self, event_logger, mock_db):
        """Test creating a plan event."""
        plan_data = {
            "plan_id": "plan_001",
            "goal": "Test goal",
            "steps": [{"step_id": "step_1", "description": "Test step"}],
        }

        event = event_logger.create_plan_event(
            user_id="user_001",
            session_id="session_001",
            event_type="plan_created",
            plan_data=plan_data,
        )

        assert event.event_id is not None
        assert event.event_type == "plan_created"
        assert event.metadata["plan_id"] == "plan_001"
        assert event.metadata["goal"] == "Test goal"
        mock_db.execute.assert_called_once()

    def test_create_plan_event_with_revision(self, event_logger, mock_db):
        """Test creating a plan revision event."""
        plan_data = {
            "plan_id": "plan_002",
            "goal": "Test goal",
            "revision_of": "plan_001",
            "steps": [],
        }

        event = event_logger.create_plan_event(
            user_id="user_001",
            session_id="session_001",
            event_type="plan_revised",
            plan_data=plan_data,
        )

        assert event.metadata["revision_of"] == "plan_001"

    def test_create_plan_event_with_causal_chain(self, event_logger, mock_db):
        """Test plan event with causal chain."""
        plan_data = {"plan_id": "plan_001", "goal": "Test", "steps": []}

        event = event_logger.create_plan_event(
            user_id="user_001",
            session_id="session_001",
            event_type="plan_created",
            plan_data=plan_data,
            parent_event_id="parent_001",
            causal_chain_id="chain_001",
        )

        assert event.parent_event_id == "parent_001"
        assert event.causal_chain_id == "chain_001"


class TestPlannerEventLogging:
    """Tests for Planner event logging methods."""

    def test_log_step_start(self, planner, mock_db):
        """Test logging step start event."""
        step = PlanStep(step_id="step_1", description="Test step")

        event_id = planner.log_step_start(
            step=step,
            plan_id="plan_001",
            user_id="user_001",
            session_id="session_001",
        )

        assert event_id is not None
        mock_db.execute.assert_called_once()

    def test_log_step_done(self, planner, mock_db):
        """Test logging step completion event."""
        step = PlanStep(
            step_id="step_1",
            description="Test step",
            status=PlanStatus.COMPLETED,
            result="Success",
            reflection="Went well",
        )

        event_id = planner.log_step_done(
            step=step,
            plan_id="plan_001",
            user_id="user_001",
            session_id="session_001",
        )

        assert event_id is not None
        mock_db.execute.assert_called_once()

    def test_log_plan_completed(self, planner, mock_db):
        """Test logging plan completion event."""
        plan = Plan(
            plan_id="plan_001",
            goal="Test goal",
            steps=[
                PlanStep(step_id="step_1", description="Step 1", status=PlanStatus.COMPLETED),
                PlanStep(step_id="step_2", description="Step 2", status=PlanStatus.COMPLETED),
            ],
        )

        event_id = planner.log_plan_completed(
            plan=plan,
            user_id="user_001",
            session_id="session_001",
            summary="All steps completed successfully",
        )

        assert event_id is not None
        mock_db.execute.assert_called_once()

    def test_log_plan_failed(self, planner, mock_db):
        """Test logging plan failure event."""
        plan = Plan(
            plan_id="plan_001",
            goal="Test goal",
            steps=[
                PlanStep(step_id="step_1", description="Step 1", status=PlanStatus.COMPLETED),
                PlanStep(step_id="step_2", description="Step 2", status=PlanStatus.FAILED),
            ],
        )

        event_id = planner.log_plan_failed(
            plan=plan,
            user_id="user_001",
            session_id="session_001",
            reason="Step 2 failed due to timeout",
        )

        assert event_id is not None
        mock_db.execute.assert_called_once()

    def test_log_plan_revised(self, planner, mock_db):
        """Test logging plan revision event."""
        revised_plan = Plan(
            plan_id="plan_002",
            goal="Test goal",
            steps=[PlanStep(step_id="step_1", description="Revised step")],
            revision_of="plan_001",
        )

        event_id = planner.log_plan_revised(
            revised_plan=revised_plan,
            user_id="user_001",
            session_id="session_001",
        )

        assert event_id is not None
        mock_db.execute.assert_called_once()

    def test_log_without_event_logger(self, mock_llm):
        """Test logging methods return None when no event logger."""
        planner = Planner(mock_llm, event_logger=None)

        step = PlanStep(step_id="step_1", description="Test")
        plan = Plan(plan_id="plan_001", goal="Test", steps=[step])

        assert planner.log_step_start(step, "plan_001", "user", "session") is None
        assert planner.log_step_done(step, "plan_001", "user", "session") is None
        assert planner.log_plan_completed(plan, "user", "session", "Done") is None
        assert planner.log_plan_failed(plan, "user", "session", "Failed") is None
        assert planner.log_plan_revised(plan, "user", "session") is None


class TestRestorePlanFromEvents:
    """Tests for restoring plan from events."""

    def test_restore_plan_basic(self, mock_db):
        """Test restoring a basic plan from events."""
        # Mock plan_created event
        mock_db.fetchall.side_effect = [
            # First call: get latest plan
            [
                {
                    "event_id": "evt_001",
                    "event_type": "plan_created",
                    "content": '{"plan_id": "plan_001", "goal": "Test goal", "steps": [{"step_id": "step_1", "description": "Test step", "status": "pending"}]}',
                    "created_at": "2024-01-01",
                    "metadata": '{"goal": "Test goal"}',
                }
            ],
            # Second call: get step events
            [],
        ]

        plan = restore_plan_from_events(mock_db, "Test goal")

        assert plan is not None
        assert plan.plan_id == "plan_001"
        assert plan.goal == "Test goal"
        assert len(plan.steps) == 1

    def test_restore_plan_with_step_progress(self, mock_db):
        """Test restoring plan with step progress."""
        mock_db.fetchall.side_effect = [
            # Latest plan
            [
                {
                    "event_id": "evt_001",
                    "event_type": "plan_created",
                    "content": '{"plan_id": "plan_001", "goal": "Test", "steps": [{"step_id": "step_1", "description": "Step 1", "status": "pending"}, {"step_id": "step_2", "description": "Step 2", "status": "pending"}]}',
                    "created_at": "2024-01-01",
                    "metadata": '{"goal": "Test"}',
                }
            ],
            # Step events
            [
                {
                    "event_type": "plan_step_start",
                    "content": '{"plan_id": "plan_001", "step_id": "step_1", "description": "Step 1"}',
                },
                {
                    "event_type": "plan_step_done",
                    "content": '{"plan_id": "plan_001", "step_id": "step_1", "status": "completed", "result": "Success"}',
                },
                {
                    "event_type": "plan_step_start",
                    "content": '{"plan_id": "plan_001", "step_id": "step_2", "description": "Step 2"}',
                },
            ],
        ]

        plan = restore_plan_from_events(mock_db, "Test")

        assert plan is not None
        # Step 1 should be completed
        assert plan.steps[0].status == "completed"
        assert plan.steps[0].result == "Success"
        # Step 2 should be in progress
        assert plan.steps[1].status == "in_progress"

    def test_restore_plan_not_found(self, mock_db):
        """Test restoring non-existent plan."""
        mock_db.fetchall.return_value = []

        plan = restore_plan_from_events(mock_db, "nonexistent")

        assert plan is None

    def test_restore_revised_plan(self, mock_db):
        """Test restoring the latest revision of a plan."""
        mock_db.fetchall.side_effect = [
            # Latest plan (revision)
            [
                {
                    "event_id": "evt_002",
                    "event_type": "plan_revised",
                    "content": '{"plan_id": "plan_002", "goal": "Test", "revision_of": "plan_001", "steps": [{"step_id": "step_1", "description": "Revised step", "status": "pending"}]}',
                    "created_at": "2024-01-02",
                    "metadata": '{"goal": "Test", "revision_of": "plan_001"}',
                }
            ],
            # Step events
            [],
        ]

        plan = restore_plan_from_events(mock_db, "Test")

        assert plan is not None
        assert plan.plan_id == "plan_002"
        assert plan.revision_of == "plan_001"


class TestCreatePlanWithEventLogging:
    """Tests for create_plan with event logging."""

    @pytest.mark.asyncio
    async def test_create_plan_logs_event(self, planner, mock_llm, mock_db):
        """Test create_plan logs plan_created event."""
        # Mock LLM response
        mock_response = MagicMock()
        mock_response.content = '{"plan_id": "plan_001", "goal": "Test", "steps": [{"step_id": "step_1", "description": "Test step"}]}'
        mock_llm.chat.return_value = mock_response

        plan = await planner.create_plan(
            goal="Test goal",
            user_id="user_001",
            session_id="session_001",
        )

        assert plan.plan_id == "plan_001"
        # Should have called db.execute for event logging
        assert mock_db.execute.called

    @pytest.mark.asyncio
    async def test_create_plan_fallback_logs_event(self, planner, mock_llm, mock_db):
        """Test create_plan fallback logs event with error metadata."""
        # Mock LLM response with invalid JSON
        mock_response = MagicMock()
        mock_response.content = "Invalid JSON"
        mock_llm.chat.return_value = mock_response

        plan = await planner.create_plan(
            goal="Test goal",
            user_id="user_001",
            session_id="session_001",
        )

        assert plan.plan_id == "plan_001"  # Fallback plan
        # Should have logged event with fallback metadata
        assert mock_db.execute.called

    @pytest.mark.asyncio
    async def test_create_plan_without_user_session(self, planner, mock_llm, mock_db):
        """Test create_plan without user/session doesn't log event."""
        mock_response = MagicMock()
        mock_response.content = '{"plan_id": "plan_001", "goal": "Test", "steps": []}'
        mock_llm.chat.return_value = mock_response

        plan = await planner.create_plan(goal="Test goal")

        assert plan.plan_id == "plan_001"
        # Should not have logged event (no user_id/session_id)
        assert not mock_db.execute.called
