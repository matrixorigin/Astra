"""Tests for PromptFeedback."""

import pytest

from core.context.prompts import PromptFeedback
from sqlalchemy.orm import Session
from api.database import get_db_session


@pytest.fixture
def db():
    """Database fixture."""
    return next(get_db_session())


@pytest.fixture
def feedback(db):
    """PromptFeedback fixture with cleanup."""
    feedback_instance = PromptFeedback(db)

    yield feedback_instance

    # Cleanup test data
    from sqlalchemy import text
    db.execute(text("DELETE FROM llm_feedback WHERE prompt_template_id LIKE 'system_test%'"))
    db.execute(text("DELETE FROM llm_feedback WHERE prompt_template_id = 'system_compare'"))
    db.commit()


def test_record_feedback(db, feedback):
    """Test recording feedback."""
    feedback_id = feedback.record_feedback(
        prompt_template_id="system_general",
        prompt_version="1.0",
        llm_request_id="test_request_123",
        user_rating=5,
        user_comment="Excellent response!",
        metadata={"task": "test"},
    )

    assert feedback_id is not None

    # Verify stored
    from sqlalchemy import text
    result = db.execute(
        text("SELECT * FROM llm_feedback WHERE feedback_id = :feedback_id"),
        {"feedback_id": feedback_id}
    )
    row = result.first()

    assert row.rating == 5
    assert row.comment == "Excellent response!"


def test_invalid_rating(feedback):
    """Test invalid rating raises error."""
    with pytest.raises(ValueError, match="Rating must be 1-5"):
        feedback.record_feedback(
            prompt_template_id="system_general",
            prompt_version="1.0",
            llm_request_id="test",
            user_rating=6,  # Invalid
        )


def test_get_feedback_stats(feedback):
    """Test getting feedback statistics."""
    # Record some feedback
    for rating in [5, 4, 5, 3, 4]:
        feedback.record_feedback(
            prompt_template_id="system_test",
            prompt_version="1.0",
            llm_request_id=f"test_{rating}",
            user_rating=rating,
        )

    # Get stats
    stats = feedback.get_feedback_stats("system_test", "1.0")

    assert stats["total_count"] == 5
    assert 4.0 <= stats["avg_rating"] <= 4.5
    assert stats["min_rating"] == 3
    assert stats["max_rating"] == 5
    assert stats["distribution"][5] == 2
    assert stats["distribution"][4] == 2
    assert stats["distribution"][3] == 1


def test_get_low_score_cases(feedback):
    """Test getting low-score cases."""
    # Record mixed feedback
    feedback.record_feedback("system_test2", "1.0", "req1", 1, "Bad")
    feedback.record_feedback("system_test2", "1.0", "req2", 2, "Poor")
    feedback.record_feedback("system_test2", "1.0", "req3", 5, "Great")

    # Get low scores
    low_scores = feedback.get_low_score_cases("system_test2", threshold=2)

    assert len(low_scores) == 2
    assert all(case["rating"] <= 2 for case in low_scores)


def test_compare_versions(feedback):
    """Test comparing two versions."""
    # Version 1.0 - lower ratings
    for rating in [2, 3, 3]:
        feedback.record_feedback("system_compare", "1.0", f"req_v1_{rating}", rating)

    # Version 1.1 - higher ratings
    for rating in [4, 5, 5]:
        feedback.record_feedback("system_compare", "1.1", f"req_v1.1_{rating}", rating)

    # Compare
    comparison = feedback.compare_versions("system_compare", "1.0", "1.1")

    assert comparison["version_a"] == "1.0"
    assert comparison["version_b"] == "1.1"
    assert comparison["stats_a"]["avg_rating"] < comparison["stats_b"]["avg_rating"]
    assert comparison["improvement"] > 0
