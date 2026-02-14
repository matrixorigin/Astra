"""Integration tests for Git for Data snapshot binding in AuditableSkillSelector."""

import pytest
import uuid
from sqlalchemy.orm import Session

from core.skills.auditable_selector import AuditableSkillSelector
from core.git_for_data import GitForData
from api.models import SkillRegistry


class TestSnapshotBinding:
    """Test real snapshot creation and binding."""

    @pytest.fixture(autouse=True)
    def cleanup_snapshots(self, db_session: Session):
        """Clean up snapshots before each test."""
        git = GitForData(db_session)
        try:
            snapshots = git.list_snapshots()
            for snapshot in snapshots:
                if snapshot["snapshot_name"].startswith("skill_select_"):
                    try:
                        git.drop_snapshot(snapshot["snapshot_name"])
                    except Exception:
                        pass  # Ignore errors during cleanup
        except Exception:
            pass
        yield

    @pytest.fixture
    def selector(self, db_session: Session):
        """Create selector with real database."""
        return AuditableSkillSelector(db_session, account="sys")

    @pytest.fixture
    def sample_skill(self, db_session: Session):
        """Create a sample skill for testing."""
        # Use unique ID for each test
        unique_id = str(uuid.uuid4())[:8]
        skill_id = f"test_skill_{unique_id}@1.0.0"
        
        skill = SkillRegistry(
            skill_id=skill_id,
            skill_name=f"test_skill_{unique_id}",
            version="1.0.0",
            skill_definition={
                "description": "Test skill for snapshot binding",
                "parameters": {},
            },
            is_active=1,
        )
        db_session.add(skill)
        db_session.commit()
        return skill

    def test_snapshot_creation_on_selection(self, selector, sample_skill, db_session):
        """Test that snapshot is created when selecting skills."""
        git = GitForData(db_session)
        
        # Get initial snapshot count
        initial_snapshots = git.list_snapshots()
        initial_count = len(initial_snapshots)
        
        # Perform selection
        event = selector.select_with_validation(
            query="test query",
            session_id="test_session",
            validate_in_sandbox=False,
        )
        
        # Verify snapshot was created
        assert event.context_snapshot.startswith("skill_select_")
        
        # Verify snapshot exists in database
        snapshots = git.list_snapshots()
        assert len(snapshots) > initial_count
        
        # Find our snapshot
        snapshot_names = [s["snapshot_name"] for s in snapshots]
        assert event.context_snapshot in snapshot_names

    def test_snapshot_binding_to_event(self, selector, sample_skill, db_session):
        """Test that snapshot ID is correctly bound to selection event."""
        # Perform selection
        event = selector.select_with_validation(
            query="test query",
            session_id="test_session_binding",
            validate_in_sandbox=False,
        )
        
        # Verify event has snapshot
        assert event.context_snapshot is not None
        assert event.context_snapshot.startswith("skill_select_")
        
        # The event should be committed by _save_event
        # Query using a fresh session to verify persistence
        from api.database import get_db_session
        fresh_db = next(get_db_session())
        try:
            from api.models import SkillSelectionEvent as EventModel
            saved_event = fresh_db.query(EventModel).filter(
                EventModel.event_id == event.event_id
            ).first()
            
            assert saved_event is not None, f"Event {event.event_id} not found in database"
            assert saved_event.context_snapshot == event.context_snapshot
            assert saved_event.session_id == "test_session_binding"
        finally:
            fresh_db.close()

    def test_time_travel_with_snapshot(self, selector, sample_skill, db_session):
        """Test that we can query data state at snapshot time."""
        git = GitForData(db_session)
        
        # Perform selection (creates snapshot)
        event = selector.select_with_validation(
            query="test query",
            session_id="test_session",
            validate_in_sandbox=False,
        )
        
        snapshot_name = event.context_snapshot
        
        # Modify data after snapshot
        sample_skill.is_active = 0
        db_session.commit()
        
        # Verify we can still access snapshot
        snapshots = git.list_snapshots()
        snapshot_names = [s["snapshot_name"] for s in snapshots]
        assert snapshot_name in snapshot_names
        
        # Note: Actual time-travel query would require raw SQL
        # This test verifies the snapshot exists and is accessible

    def test_snapshot_fallback_on_error(self, selector, db_session, monkeypatch):
        """Test that selector falls back gracefully if snapshot creation fails."""
        # Mock GitForData to raise error
        def mock_create_snapshot(*args, **kwargs):
            raise Exception("Snapshot creation failed")
        
        from core import git_for_data
        monkeypatch.setattr(git_for_data.GitForData, "create_snapshot", mock_create_snapshot)
        
        # Selection should still work with fallback
        event = selector.select_with_validation(
            query="test query",
            session_id="test_session",
            validate_in_sandbox=False,
        )
        
        # Should have fallback snapshot ID
        assert event.context_snapshot.startswith("snapshot_")
        assert "T" in event.context_snapshot  # ISO timestamp format

    def test_multiple_selections_create_unique_snapshots(self, selector, sample_skill, db_session):
        """Test that each selection creates a unique snapshot."""
        git = GitForData(db_session)
        
        # Perform multiple selections
        event1 = selector.select_with_validation(
            query="query 1",
            session_id="session_1",
            validate_in_sandbox=False,
        )
        
        event2 = selector.select_with_validation(
            query="query 2",
            session_id="session_2",
            validate_in_sandbox=False,
        )
        
        # Verify unique snapshots
        assert event1.context_snapshot != event2.context_snapshot
        
        # Verify both exist
        snapshots = git.list_snapshots()
        snapshot_names = [s["snapshot_name"] for s in snapshots]
        assert event1.context_snapshot in snapshot_names
        assert event2.context_snapshot in snapshot_names

    def test_snapshot_contains_event_id(self, selector, sample_skill, db_session):
        """Test that snapshot name contains event ID for traceability."""
        event = selector.select_with_validation(
            query="test query",
            session_id="test_session_event_id",
            validate_in_sandbox=False,
        )
        
        # Snapshot should contain event ID (with hyphens replaced by underscores)
        event_id_normalized = event.event_id.replace('-', '_')
        assert event_id_normalized in event.context_snapshot
        assert event.context_snapshot == f"skill_select_{event_id_normalized}"
