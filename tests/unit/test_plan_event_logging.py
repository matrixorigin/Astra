"""Unit tests for plan event logging."""

import json
import pytest
from sqlalchemy import delete
from unittest.mock import MagicMock

from core.agent.planner import Plan, PlanStatus, PlanStep, Planner, restore_plan_from_events
from core.events.event_logger import EventLogger
from api.database import get_db_session
from api.models import Event


@pytest.fixture
def db():
    """Get test database session."""
    session = next(get_db_session())
    # Clean up before test
    session.execute(delete(Event))
    session.commit()
    yield session
    # Clean up after test
    session.execute(delete(Event))
    session.commit()
    session.close()


@pytest.fixture
def event_logger(db):
    """EventLogger with real database session."""
    return EventLogger.from_session(db)


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

    def test_create_plan_event(self, event_logger, db):
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
        metadata = (
            event.metadata if isinstance(event.metadata, dict) else json.loads(event.metadata)
        )
        assert metadata["plan_id"] == "plan_001"
        assert metadata["goal"] == "Test goal"

    def test_create_plan_event_with_revision(self, event_logger, db):
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

        metadata = (
            event.metadata if isinstance(event.metadata, dict) else json.loads(event.metadata)
        )
        assert metadata["revision_of"] == "plan_001"

    def test_create_plan_event_with_causal_chain(self, event_logger, db):
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
    """Tests for planner event logging."""

    def test_log_step_start(self, planner):
        """Test logging step start."""
        step = PlanStep(step_id="step_1", description="Test step")
        plan = Plan(plan_id="plan_001", goal="Test", steps=[step])

        event_id = planner.log_step_start(step, "plan_001", "user", "session")
        assert event_id is not None

    def test_log_step_done(self, planner):
        """Test logging step done."""
        step = PlanStep(step_id="step_1", description="Test step")
        plan = Plan(plan_id="plan_001", goal="Test", steps=[step])

        event_id = planner.log_step_done(step, "plan_001", "user", "session")
        assert event_id is not None

    def test_log_plan_completed(self, planner):
        """Test logging plan completed."""
        step = PlanStep(step_id="step_1", description="Test step")
        plan = Plan(plan_id="plan_001", goal="Test", steps=[step])

        event_id = planner.log_plan_completed(plan, "user", "session", "Done")
        assert event_id is not None

    def test_log_plan_failed(self, planner):
        """Test logging plan failed."""
        step = PlanStep(step_id="step_1", description="Test step")
        plan = Plan(plan_id="plan_001", goal="Test", steps=[step])

        event_id = planner.log_plan_failed(plan, "user", "session", "Failed")
        assert event_id is not None

    def test_log_plan_revised(self, planner):
        """Test logging plan revised."""
        step = PlanStep(step_id="step_1", description="Test step")
        plan = Plan(plan_id="plan_001", goal="Test", steps=[step])

        event_id = planner.log_plan_revised(plan, "user", "session")
        assert event_id is not None

    def test_log_without_event_logger(self, mock_llm):
        """Test planner without event logger."""
        planner = Planner(mock_llm, event_logger=None)
        step = PlanStep(step_id="step_1", description="Test step")
        plan = Plan(plan_id="plan_001", goal="Test", steps=[step])

        # Should not raise error
        result = planner.log_step_start(step, "plan_001", "user", "session")
        assert result is None


class TestRestorePlanFromEvents:
    """Tests for restoring plan from events."""

    def test_restore_plan_basic(self, db):
        """Test restoring a basic plan from events."""
        from uuid_utils import uuid7

        # Create plan event - use ORM attribute names
        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_created",
                content=json.dumps(
                    {
                        "plan_id": "plan_001",
                        "goal": "Test goal",
                        "steps": [
                            {"step_id": "step_1", "description": "Test step", "status": "pending"}
                        ],
                    }
                ),
                event_metadata={"goal": "Test goal"},  # Use ORM attribute, dict not string
                user_id="user_001",
                causal_chain_id=str(uuid7()),
            )
        )
        db.commit()

        plan = restore_plan_from_events(db, "Test goal")

        assert plan is not None
        assert plan.plan_id == "plan_001"
        assert plan.goal == "Test goal"
        assert len(plan.steps) == 1

    def test_restore_plan_with_step_progress(self, db):
        """Test restoring plan with step progress."""
        from uuid_utils import uuid7

        chain_id = str(uuid7())

        # Create plan event
        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_created",
                content=json.dumps(
                    {
                        "plan_id": "plan_001",
                        "goal": "Test",
                        "steps": [
                            {"step_id": "step_1", "description": "Step 1", "status": "pending"},
                            {"step_id": "step_2", "description": "Step 2", "status": "pending"},
                        ],
                    }
                ),
                event_metadata={"goal": "Test"},
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )

        # Create step events
        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_step_start",
                content=json.dumps(
                    {"plan_id": "plan_001", "step_id": "step_1", "description": "Step 1"}
                ),
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )

        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_step_done",
                content=json.dumps(
                    {
                        "plan_id": "plan_001",
                        "step_id": "step_1",
                        "status": "completed",
                        "result": "Success",
                    }
                ),
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )

        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_step_start",
                content=json.dumps(
                    {"plan_id": "plan_001", "step_id": "step_2", "description": "Step 2"}
                ),
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )

        db.commit()

        plan = restore_plan_from_events(db, "Test")

        assert plan is not None
        # Step 1 should be completed
        assert plan.steps[0].status == "completed"
        assert plan.steps[0].result == "Success"
        # Step 2 should be in progress
        assert plan.steps[1].status == "in_progress"

    def test_restore_plan_not_found(self, db):
        """Test restoring non-existent plan."""
        plan = restore_plan_from_events(db, "nonexistent")
        assert plan is None

    def test_restore_revised_plan(self, db):
        """Test restoring the latest revision of a plan."""
        from uuid_utils import uuid7

        chain_id = str(uuid7())

        # Create original plan
        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_created",
                content=json.dumps(
                    {
                        "plan_id": "plan_001",
                        "goal": "Test",
                        "steps": [
                            {
                                "step_id": "step_1",
                                "description": "Original step",
                                "status": "pending",
                            }
                        ],
                    }
                ),
                event_metadata={"goal": "Test"},
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )

        # Create revised plan
        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_revised",
                content=json.dumps(
                    {
                        "plan_id": "plan_002",
                        "goal": "Test",
                        "revision_of": "plan_001",
                        "steps": [
                            {
                                "step_id": "step_1",
                                "description": "Revised step",
                                "status": "pending",
                            }
                        ],
                    }
                ),
                event_metadata={"goal": "Test", "revision_of": "plan_001"},
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )

        db.commit()

        plan = restore_plan_from_events(db, "Test")

        assert plan is not None
        assert plan.plan_id == "plan_002"
        assert plan.steps[0].description == "Revised step"

    def test_restore_skips_completed_plan(self, db):
        """Completed plans should not be restored."""
        from uuid_utils import uuid7

        chain_id = str(uuid7())

        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_created",
                content=json.dumps(
                    {
                        "plan_id": "plan_done",
                        "goal": "Finished goal",
                        "steps": [{"step_id": "s1", "description": "Done", "status": "completed"}],
                    }
                ),
                event_metadata={"goal": "Finished goal"},
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )
        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_completed",
                content=json.dumps({"plan_id": "plan_done", "summary": "all done"}),
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )
        db.commit()

        plan = restore_plan_from_events(db, "Finished goal")
        assert plan is None

    def test_restore_skips_failed_plan(self, db):
        """Failed plans should not be restored."""
        from uuid_utils import uuid7

        chain_id = str(uuid7())

        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_created",
                content=json.dumps(
                    {
                        "plan_id": "plan_fail",
                        "goal": "Failed goal",
                        "steps": [{"step_id": "s1", "description": "Fail", "status": "pending"}],
                    }
                ),
                event_metadata={"goal": "Failed goal"},
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )
        db.add(
            Event(
                event_id=str(uuid7()),
                session_id="session_001",
                event_type="plan_failed",
                content=json.dumps({"plan_id": "plan_fail", "reason": "constraint violation"}),
                user_id="user_001",
                causal_chain_id=chain_id,
            )
        )
        db.commit()

        plan = restore_plan_from_events(db, "Failed goal")
        assert plan is None


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
