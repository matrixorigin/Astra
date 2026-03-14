"""Integration tests for P1 Evaluation Loop — multi-dimensional quality scoring and golden sessions.

Tests:
1. Multi-dimensional quality scoring (accuracy, latency, cost, satisfaction, trust)
2. Golden sessions selection and tagging
3. Golden set versioning and retrieval
4. Regression gate auto-triggering on changes
"""

import json
from datetime import datetime, timezone

import pytest
from sqlalchemy import text

from core.evaluation.quality_scorer import (
    QualityDimension,
    QualityScorer,
    QualityWeights,
)
from core.evaluation.golden_selector import GoldenSessionSelector
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from api.database import get_db_session


@pytest.fixture
def db_session():
    """Get database session."""
    session = next(get_db_session())
    yield session
    session.close()


@pytest.fixture
def session_manager(db_session):
    """Create session manager."""
    return SessionManager(db_session)


@pytest.fixture
def event_logger(db_session):
    """Create event logger."""
    return EventLogger.from_session(db_session)


@pytest.fixture
def quality_scorer():
    """Create quality scorer."""
    return QualityScorer()


@pytest.fixture
def golden_selector(db_session):
    """Create golden selector."""
    return GoldenSessionSelector(lambda: db_session)


class TestQualityScorer:
    """Test multi-dimensional quality scoring."""

    def test_accuracy_scoring_exact_match(self, quality_scorer):
        """Exact match should score 1.0."""
        score = quality_scorer.score_accuracy(
            expected_output="The answer is 42",
            actual_output="The answer is 42",
        )

        assert score.dimension == QualityDimension.ACCURACY
        assert score.score == 1.0

    def test_accuracy_scoring_partial_match(self, quality_scorer):
        """Partial match should score 0.7."""
        score = quality_scorer.score_accuracy(
            expected_output="The answer is 42",
            actual_output="The answer is 42 and more details",
        )

        assert score.score == 0.7

    def test_accuracy_scoring_mismatch(self, quality_scorer):
        """Mismatch should score 0.0."""
        score = quality_scorer.score_accuracy(
            expected_output="The answer is 42",
            actual_output="The answer is 43",
        )

        assert score.score == 0.0

    def test_latency_scoring_within_slo(self, quality_scorer):
        """Latency within SLO should score 1.0."""
        score = quality_scorer.score_latency(
            execution_time_ms=3000,
            slo_ms=5000,
        )

        assert score.dimension == QualityDimension.LATENCY
        assert score.score == 1.0

    def test_latency_scoring_slightly_over_slo(self, quality_scorer):
        """Latency slightly over SLO should score 0.7."""
        score = quality_scorer.score_latency(
            execution_time_ms=6000,
            slo_ms=5000,
        )

        assert score.score == 0.7

    def test_latency_scoring_way_over_slo(self, quality_scorer):
        """Latency way over SLO should score low."""
        score = quality_scorer.score_latency(
            execution_time_ms=15000,
            slo_ms=5000,
        )

        assert score.score < 0.5

    def test_cost_scoring_within_budget(self, quality_scorer):
        """Cost within budget should score 1.0."""
        score = quality_scorer.score_cost(
            actual_cost=0.05,
            budget_cost=0.10,
        )

        assert score.dimension == QualityDimension.COST
        assert score.score == 1.0

    def test_cost_scoring_slight_overrun(self, quality_scorer):
        """Cost with slight overrun should score 0.7."""
        score = quality_scorer.score_cost(
            actual_cost=0.11,
            budget_cost=0.10,
            overrun_factor=1.2,
        )

        assert score.score == 0.7

    def test_satisfaction_scoring_with_rating(self, quality_scorer):
        """User rating should be used directly."""
        score = quality_scorer.score_satisfaction(user_rating=0.85)

        assert score.dimension == QualityDimension.SATISFACTION
        assert score.score == 0.85

    def test_satisfaction_scoring_with_sentiment(self, quality_scorer):
        """Sentiment should map to score."""
        positive = quality_scorer.score_satisfaction(feedback_sentiment="positive")
        neutral = quality_scorer.score_satisfaction(feedback_sentiment="neutral")
        negative = quality_scorer.score_satisfaction(feedback_sentiment="negative")

        assert positive.score > neutral.score > negative.score

    def test_trust_scoring_above_threshold(self, quality_scorer):
        """Confidence above threshold should score high."""
        score = quality_scorer.score_trust(
            confidence_score=0.85,
            threshold=0.7,
        )

        assert score.dimension == QualityDimension.TRUST
        assert score.score == 0.85

    def test_trust_scoring_below_threshold(self, quality_scorer):
        """Confidence below threshold should score lower."""
        score = quality_scorer.score_trust(
            confidence_score=0.5,
            threshold=0.7,
        )

        assert score.score < 0.5

    def test_overall_score_computation(self, quality_scorer):
        """Overall score should be weighted average."""
        dimensions = [
            quality_scorer.score_accuracy("expected", "expected"),  # 1.0
            quality_scorer.score_latency(3000, 5000),  # 1.0
            quality_scorer.score_cost(0.05, 0.10),  # 1.0
            quality_scorer.score_satisfaction(user_rating=0.8),  # 0.8
            quality_scorer.score_trust(0.85, 0.7),  # 0.85
        ]

        result = quality_scorer.compute_overall_score(dimensions)

        # Should be weighted average
        assert 0.8 < result.overall_score <= 1.0
        assert result.training_eligible is True

    def test_custom_weights(self):
        """Custom weights should be applied."""
        weights = QualityWeights(
            accuracy_weight=0.6,
            latency_weight=0.2,
            cost_weight=0.1,
            satisfaction_weight=0.05,
            trust_weight=0.05,
        )
        scorer = QualityScorer(weights)

        dimensions = [
            scorer.score_accuracy("expected", "expected"),  # 1.0
            scorer.score_latency(10000, 5000),  # ~0.0
        ]

        result = scorer.compute_overall_score(dimensions)

        # Accuracy dominates (0.6 weight), latency low (0.2 weight)
        assert result.overall_score > 0.5


class TestGoldenSessionSelector:
    """Test golden sessions selection and management."""

    def test_golden_selector_initialization(self, db_session):
        """Should initialize golden selector."""
        selector = GoldenSessionSelector(lambda: db_session)
        assert selector is not None
        assert selector._db_factory is not None

    def test_tag_golden_session(
        self,
        db_session,
        session_manager,
        event_logger,
        golden_selector,
    ):
        """Should tag session as golden."""
        session = session_manager.create_session(user_id="test_user")
        user_event = event_logger.create_user_query(
            user_id="test_user",
            session_id=session.session_id,
            content="Test",
        )

        response_event = event_logger.create_llm_response(
            user_id="test_user",
            session_id=session.session_id,
            content="Response",
            agent_id="dev-agent",
            agent_version="0.1.0",
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        # Tag as golden
        golden_set_id = "golden_set_001"
        success = golden_selector.tag_golden_session(
            event_id=response_event.event_id,
            golden_set_id=golden_set_id,
            reason="High quality",
        )

        assert success is True

    def test_create_golden_set(
        self,
        db_session,
        session_manager,
        event_logger,
        golden_selector,
    ):
        """Should create versioned golden set."""
        # Create sessions
        sessions_data = []
        for i in range(3):
            session = session_manager.create_session(user_id=f"user_{i}")
            user_event = event_logger.create_user_query(
                user_id=f"user_{i}",
                session_id=session.session_id,
                content="Test",
            )

            response_event = event_logger.create_llm_response(
                user_id=f"user_{i}",
                session_id=session.session_id,
                content=f"Response {i}",
                agent_id="dev-agent",
                agent_version="0.1.0",
                parent_event_id=user_event.event_id,
                causal_chain_id=user_event.causal_chain_id,
            )

            db_session.execute(
                text("""
                    UPDATE agent_events 
                    SET quality_score = :score 
                    WHERE event_id = :event_id
                """),
                {"score": 4.5, "event_id": response_event.event_id},
            )

            sessions_data.append(
                {
                    "event_id": response_event.event_id,
                    "session_id": session.session_id,
                    "quality_score": 4.5,
                }
            )

        db_session.commit()

        # Create golden set
        golden_set_id = golden_selector.create_golden_set(
            sessions=sessions_data,
            name="test_golden_set",
            description="Test golden set",
        )

        assert golden_set_id is not None
        assert len(golden_set_id) > 0

    def test_get_golden_set(self, golden_selector):
        """Should retrieve sessions from golden set."""
        # Just test that the method exists and returns a list
        sessions = golden_selector.get_golden_set("nonexistent_set")
        assert isinstance(sessions, list)

    def test_list_golden_sets(self, golden_selector):
        """Should list golden sets."""
        # Just test that the method exists and returns a list
        sets = golden_selector.list_golden_sets()
        assert isinstance(sets, list)


class TestEvaluationLoopIntegration:
    """Integration tests for full evaluation loop."""

    def test_quality_scoring_pipeline(
        self,
        db_session,
        session_manager,
        event_logger,
        quality_scorer,
    ):
        """Full quality scoring pipeline."""
        session = session_manager.create_session(user_id="pipeline_test")
        user_event = event_logger.create_user_query(
            user_id="pipeline_test",
            session_id=session.session_id,
            content="What is 2+2?",
        )

        response_event = event_logger.create_llm_response(
            user_id="pipeline_test",
            session_id=session.session_id,
            content="2+2 equals 4",
            agent_id="dev-agent",
            agent_version="0.1.0",
            parent_event_id=user_event.event_id,
            causal_chain_id=user_event.causal_chain_id,
        )

        # Score across dimensions
        dimensions = [
            quality_scorer.score_accuracy("2+2 equals 4", "2+2 equals 4"),
            quality_scorer.score_latency(2000, 5000),
            quality_scorer.score_cost(0.01, 0.05),
            quality_scorer.score_satisfaction(user_rating=0.9),
            quality_scorer.score_trust(0.85, 0.7),
        ]

        result = quality_scorer.compute_overall_score(dimensions)

        # Should have high overall score
        assert result.overall_score > 0.8
        assert result.training_eligible is True
