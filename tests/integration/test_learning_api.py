"""Test learning service API."""

import pytest
from fastapi.testclient import TestClient
from datetime import datetime, timezone
from uuid_utils import uuid7

from api.main import app
from api.database import get_db_session
from api.models import SkillSelectionEvent


@pytest.fixture
def client():
    """Test client."""
    return TestClient(app)


@pytest.fixture
def db():
    """Database session."""
    return next(get_db_session())


class TestLearningAPI:
    """Test learning service API endpoints."""

    def test_health_check(self, client):
        """Test health check endpoint."""
        response = client.get("/api/v1/learning/health")
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "healthy"
        assert data["service"] == "learning"

    def test_get_stats(self, client):
        """Test get learning statistics."""
        response = client.get("/api/v1/learning/stats")
        assert response.status_code == 200
        data = response.json()
        assert "total_learnings" in data
        assert "high_confidence" in data
        assert "total_gates" in data
        assert "pass_rate" in data

    def test_trigger_learning_no_data(self, client):
        """Test trigger learning with no failure data."""
        response = client.post(
            "/api/v1/learning/trigger",
            json={"days": 7, "force": False}
        )
        assert response.status_code == 200
        data = response.json()
        assert data["status"] in ["success", "error"]
        assert "learned" in data

    def test_trigger_learning_with_force(self, client):
        """Test trigger learning with force flag."""
        response = client.post(
            "/api/v1/learning/trigger",
            json={"days": 7, "force": True}
        )
        assert response.status_code == 200
        data = response.json()
        assert "learned" in data

    def test_submit_feedback(self, client, db):
        """Test submit feedback for event."""
        # Create test event
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="Test query",
            context_snapshot="snapshot_123",
            available_skills=[],
            selected_skills=["test_skill"],
            selection_method="test",
            selection_reasoning="Test",
            candidate_scores={},
            created_at=datetime.now(timezone.utc),
        )
        db.add(event)
        db.commit()
        
        # Submit feedback
        response = client.post(
            "/api/v1/learning/feedback",
            json={
                "event_id": event.event_id,
                "feedback_type": "wrong_skill",
                "correct_skills": ["correct_skill"],
                "satisfaction_score": 2
            }
        )
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "success"
        
        # Verify event updated
        db.refresh(event)
        assert event.selection_correctness == 0
        assert event.correction_suggestion == ["correct_skill"]
        assert event.user_feedback_score == 2
        
        # Cleanup
        db.delete(event)
        db.commit()

    def test_submit_feedback_not_found(self, client):
        """Test submit feedback for non-existent event."""
        response = client.post(
            "/api/v1/learning/feedback",
            json={
                "event_id": "non_existent",
                "feedback_type": "wrong_skill",
                "correct_skills": ["correct_skill"]
            }
        )
        assert response.status_code == 404

    def test_trigger_learning_validation(self, client):
        """Test request validation."""
        # Invalid days (too large)
        response = client.post(
            "/api/v1/learning/trigger",
            json={"days": 100}
        )
        assert response.status_code == 422
        
        # Invalid days (negative)
        response = client.post(
            "/api/v1/learning/trigger",
            json={"days": -1}
        )
        assert response.status_code == 422


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
