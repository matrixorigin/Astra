"""Test configurable relevance scorer."""

import pytest
from sqlalchemy.orm import Session

from api.database import get_db_session
from core.context.manager import TaskType
from core.context.scorer import (
    TASK_WEIGHTS,
    RelevanceScorer,
    ScoringWeights,
    create_scorer_for_task,
)
from core.events.event_logger import EventLogger


@pytest.fixture
def db():
    """SQLAlchemy Session fixture."""
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def event_logger(db):
    """Event logger fixture."""
    return EventLogger(db)


def test_scoring_weights_validation():
    """Test that weights must sum to 1.0."""
    # Valid weights
    weights = ScoringWeights(semantic=0.4, temporal=0.2, causal=0.3, keyword=0.1)
    assert weights.semantic == 0.4

    # Invalid weights (don't sum to 1.0)
    with pytest.raises(ValueError, match="must sum to 1.0"):
        ScoringWeights(semantic=0.5, temporal=0.5, causal=0.5, keyword=0.5)


def test_task_specific_weights():
    """Test that each task type has valid weights."""
    for task_type, weights in TASK_WEIGHTS.items():
        total = weights.semantic + weights.temporal + weights.causal + weights.keyword
        assert abs(total - 1.0) < 0.01, f"{task_type} weights don't sum to 1.0"


def test_scorer_initialization(db):
    """Test scorer initialization."""
    from core.context.embeddings import EmbeddingService

    embeddings = EmbeddingService(db, provider="mock")
    scorer = RelevanceScorer(db, embeddings)

    assert scorer.db is not None
    assert scorer.embeddings is not None
    assert scorer.weights is not None


def test_scorer_with_custom_weights(db):
    """Test scorer with custom weights."""
    from core.context.embeddings import EmbeddingService

    embeddings = EmbeddingService(db, provider="mock")

    custom_weights = ScoringWeights(semantic=0.5, temporal=0.1, causal=0.3, keyword=0.1)

    scorer = RelevanceScorer(db, embeddings, weights=custom_weights)
    assert scorer.weights.semantic == 0.5
    assert scorer.weights.temporal == 0.1


def test_score_candidates_basic(db, event_logger):
    """Test basic candidate scoring."""
    from api.models import Event as EventModel
    from core.context.embeddings import EmbeddingService

    session_id = "test_session_scorer_001"
    user_id = "test_user"

    # Create test events
    event1 = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="First query about Python"
    )

    event_logger.create_llm_response(
        user_id=user_id,
        session_id=session_id,
        content="Response about Python",
        agent_id="test-agent",
        agent_version="1.0",
        parent_event_id=event1.event_id,
        causal_chain_id=event1.causal_chain_id,
    )

    event2 = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Second query about JavaScript"
    )

    # Get candidates using SQLAlchemy
    events = db.query(EventModel).filter(EventModel.session_id == session_id).order_by(EventModel.created_at.desc()).all()
    
    candidates = [
        {
            "event_id": e.event_id,
            "event_type": e.event_type,
            "content": e.content,
            "created_at": e.created_at,
            "parent_event_id": e.parent_event_id,
            "causal_chain_id": e.causal_chain_id,
            "metadata": e.event_metadata,
        }
        for e in events
    ]

    # Score candidates
    embeddings = EmbeddingService(db, provider="mock")
    scorer = RelevanceScorer(db, embeddings)

    scored = scorer.score_candidates("Python", candidates, session_id, TaskType.GENERAL)

    # Verify results
    assert len(scored) == len(candidates)
    assert all(len(item) == 3 for item in scored)  # (candidate, score, signals)

    # Verify scores are between 0 and 1
    for _candidate, score, signals in scored:
        assert 0.0 <= score <= 1.0
        assert "semantic" in signals
        assert "temporal" in signals
        assert "causal" in signals
        assert "keyword" in signals


def test_task_specific_scoring(db, event_logger):
    """Test that different task types produce different scores."""
    from api.models import Event as EventModel
    from core.context.embeddings import EmbeddingService

    session_id = "test_session_scorer_002"
    user_id = "test_user"

    # Create test event
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Code review query"
    )

    # Get candidates using SQLAlchemy
    events = db.query(EventModel).filter(EventModel.session_id == session_id).all()
    
    candidates = [
        {
            "event_id": e.event_id,
            "event_type": e.event_type,
            "content": e.content,
            "created_at": e.created_at,
            "parent_event_id": e.parent_event_id,
            "causal_chain_id": e.causal_chain_id,
            "metadata": e.event_metadata,
        }
        for e in events
    ]

    embeddings = EmbeddingService(db, provider="mock")
    scorer = RelevanceScorer(db, embeddings)

    # Score with different task types
    general_scored = scorer.score_candidates("code", candidates, session_id, TaskType.GENERAL)

    code_review_scored = scorer.score_candidates(
        "code", candidates, session_id, TaskType.CODE_REVIEW
    )

    # Scores should be different due to different weights
    general_score = general_scored[0][1]
    code_review_score = code_review_scored[0][1]

    # Both should be valid scores
    assert 0.0 <= general_score <= 1.0
    assert 0.0 <= code_review_score <= 1.0


def test_create_scorer_for_task(db):
    """Test factory function for task-specific scorers."""
    from core.context.embeddings import EmbeddingService

    embeddings = EmbeddingService(db, provider="mock")

    # Create scorers for different tasks
    code_scorer = create_scorer_for_task(db, embeddings, TaskType.CODE_REVIEW)
    planning_scorer = create_scorer_for_task(db, embeddings, TaskType.PLANNING)

    # Verify they have different weights
    assert code_scorer.weights.semantic != planning_scorer.weights.semantic


def test_scorer_empty_query(db, event_logger):
    """Test scorer with empty query."""
    from api.models import Event as EventModel
    from core.context.embeddings import EmbeddingService

    session_id = "test_session_scorer_004"
    user_id = "test_user"

    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Test content"
    )

    # Get candidates using SQLAlchemy
    events = db.query(EventModel).filter(EventModel.session_id == session_id).all()
    candidates = [
        {
            "event_id": e.event_id,
            "event_type": e.event_type,
            "content": e.content,
            "created_at": e.created_at,
            "parent_event_id": e.parent_event_id,
            "causal_chain_id": e.causal_chain_id,
            "metadata": e.event_metadata,
        }
        for e in events
    ]

    embeddings = EmbeddingService(db, provider="mock")
    scorer = RelevanceScorer(db, embeddings)

    # Empty query
    scored = scorer.score_candidates("", candidates, session_id, TaskType.GENERAL)

    # Should return candidates with zero scores
    assert len(scored) == len(candidates)
    assert all(score == 0.0 for _, score, _ in scored)


def test_scorer_empty_candidates(db):
    """Test scorer with no candidates."""
    from core.context.embeddings import EmbeddingService

    embeddings = EmbeddingService(db, provider="mock")
    scorer = RelevanceScorer(db, embeddings)

    scored = scorer.score_candidates("test query", [], "session_123", TaskType.GENERAL)

    assert len(scored) == 0


def test_scorer_empty_session_id(db, event_logger):
    """Test scorer with empty session_id."""
    from api.models import Event as EventModel
    from core.context.embeddings import EmbeddingService

    session_id = "test_session_scorer_005"
    user_id = "test_user"

    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Test content"
    )

    # Get candidates using SQLAlchemy
    events = db.query(EventModel).filter(EventModel.session_id == session_id).all()
    candidates = [
        {
            "event_id": e.event_id,
            "event_type": e.event_type,
            "content": e.content,
            "created_at": e.created_at,
            "parent_event_id": e.parent_event_id,
            "causal_chain_id": e.causal_chain_id,
            "metadata": e.event_metadata,
        }
        for e in events
    ]

    embeddings = EmbeddingService(db, provider="mock")
    scorer = RelevanceScorer(db, embeddings)

    # Empty session_id
    scored = scorer.score_candidates("test", candidates, "", TaskType.GENERAL)

    # Should return candidates with zero scores
    assert len(scored) == len(candidates)


def test_signal_breakdown(db, event_logger):
    """Test that signal breakdown is returned correctly."""
    from api.models import Event as EventModel
    from core.context.embeddings import EmbeddingService

    session_id = "test_session_scorer_003"
    user_id = "test_user"

    # Create test event
    event = event_logger.create_user_query(
        user_id=user_id, session_id=session_id, content="Test query with keyword"
    )

    # Get candidates using SQLAlchemy
    events = db.query(EventModel).filter(EventModel.session_id == session_id).all()
    candidates = [
        {
            "event_id": e.event_id,
            "event_type": e.event_type,
            "content": e.content,
            "created_at": e.created_at,
            "parent_event_id": e.parent_event_id,
            "causal_chain_id": e.causal_chain_id,
            "metadata": e.event_metadata,
        }
        for e in events
    ]

    embeddings = EmbeddingService(db, provider="mock")
    scorer = RelevanceScorer(db, embeddings)

    scored = scorer.score_candidates("keyword", candidates, session_id, TaskType.GENERAL)

    # Verify signal breakdown
    _candidate, total_score, signals = scored[0]

    # All signals should be present
    assert "semantic" in signals
    assert "temporal" in signals
    assert "causal" in signals
    assert "keyword" in signals

    # Total score should equal sum of signals
    signal_sum = sum(signals.values())
    assert abs(total_score - signal_sum) < 0.01


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
