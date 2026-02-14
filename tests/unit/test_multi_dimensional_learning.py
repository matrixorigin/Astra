"""Tests for multi-dimensional learning signals (Phase 1)."""

import pytest
from datetime import datetime, timezone

from core.skills.learning_signals import LearningSignal, SignalType, SignalWeights
from core.skills.self_improving_selector import SelfImprovingSelector
from api.models import SkillSelectionEvent as EventModel


class TestLearningSignals:
    """Test learning signal extraction and processing."""
    
    def test_signal_type_enum(self):
        """Test signal type enum values."""
        assert SignalType.WRONG_SKILL.value == "wrong_skill"
        assert SignalType.SLOW_EXECUTION.value == "slow_execution"
        assert SignalType.HIGH_COST.value == "high_cost"
        assert SignalType.LOW_SATISFACTION.value == "low_satisfaction"
    
    def test_learning_signal_creation(self):
        """Test LearningSignal dataclass."""
        signal = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="create a PR",
            wrong_skills=["github_list_repos"],
            correct_skills=["github_create_pr"],
            target_metrics={"accuracy": 1.0},
        )
        
        assert signal.signal_type == SignalType.WRONG_SKILL
        assert signal.query_pattern == "create a PR"
        assert signal.confidence == 10.0
        
        data = signal.to_dict()
        assert data["signal_type"] == "wrong_skill"
        assert data["target_metrics"]["accuracy"] == 1.0
    
    def test_signal_weights_validation(self):
        """Test signal weights validation."""
        # Valid weights
        weights = SignalWeights(accuracy=0.4, speed=0.3, cost=0.2, satisfaction=0.1)
        assert weights.accuracy == 0.4
        
        # Invalid weights (don't sum to 1.0)
        with pytest.raises(ValueError, match="must sum to 1.0"):
            SignalWeights(accuracy=0.5, speed=0.5, cost=0.5, satisfaction=0.5)
        
        # Negative weight
        with pytest.raises(ValueError, match="cannot be negative"):
            SignalWeights(accuracy=-0.1, speed=0.4, cost=0.4, satisfaction=0.3)
        
        # Weight > 1.0
        with pytest.raises(ValueError, match="cannot exceed 1.0"):
            SignalWeights(accuracy=1.5, speed=0.0, cost=0.0, satisfaction=-0.5)
    
    def test_signal_weights_to_dict(self):
        """Test weights serialization."""
        weights = SignalWeights()
        data = weights.to_dict()
        
        assert data["accuracy"] == 0.4
        assert data["speed"] == 0.3
        assert data["cost"] == 0.2
        assert data["satisfaction"] == 0.1


class TestMultiDimensionalLearning:
    """Test multi-dimensional learning in SelfImprovingSelector."""
    
    @pytest.fixture
    def selector(self, db_session):
        """Create selector with custom weights."""
        weights = SignalWeights(accuracy=0.5, speed=0.2, cost=0.2, satisfaction=0.1)
        return SelfImprovingSelector(db_session, llm_client=None, weights=weights)
    
    def test_extract_wrong_skill_signal(self, selector):
        """Test extracting wrong_skill signal."""
        failure = {
            "user_query": "Create a pull request",
            "selected_skills": ["github_list_repos"],
            "correction_suggestion": ["github_create_pr"],
        }
        
        signal = selector._extract_signal(failure, SignalType.WRONG_SKILL)
        
        assert signal is not None
        assert signal.signal_type == SignalType.WRONG_SKILL
        assert signal.wrong_skills == ["github_list_repos"]
        assert signal.correct_skills == ["github_create_pr"]
        assert signal.target_metrics["accuracy"] == 1.0
    
    def test_extract_slow_execution_signal(self, selector):
        """Test extracting slow_execution signal."""
        failure = {
            "user_query": "Run tests",
            "selected_skills": ["pytest_run_all"],
            "execution_time_ms": 10000,  # 10 seconds
        }
        
        signal = selector._extract_signal(failure, SignalType.SLOW_EXECUTION)
        
        assert signal is not None
        assert signal.signal_type == SignalType.SLOW_EXECUTION
        assert signal.target_metrics["time_ms"] == 5000  # Target: 50% faster
    
    def test_extract_slow_execution_below_threshold(self, selector):
        """Test slow execution below threshold returns None."""
        failure = {
            "user_query": "Quick task",
            "selected_skills": ["fast_skill"],
            "execution_time_ms": 1000,  # 1 second (below 5s threshold)
        }
        
        signal = selector._extract_signal(failure, SignalType.SLOW_EXECUTION)
        assert signal is None
    
    def test_extract_high_cost_signal(self, selector):
        """Test extracting high_cost signal."""
        failure = {
            "user_query": "Analyze large dataset",
            "selected_skills": ["gpt4_analyze"],
            "execution_cost": 0.50,  # $0.50
        }
        
        signal = selector._extract_signal(failure, SignalType.HIGH_COST)
        
        assert signal is not None
        assert signal.signal_type == SignalType.HIGH_COST
        assert signal.target_metrics["cost"] == 0.25  # Target: 50% cheaper
    
    def test_extract_high_cost_below_threshold(self, selector):
        """Test high cost below threshold returns None."""
        failure = {
            "user_query": "Simple task",
            "selected_skills": ["cheap_skill"],
            "execution_cost": 0.05,  # $0.05 (below $0.10 threshold)
        }
        
        signal = selector._extract_signal(failure, SignalType.HIGH_COST)
        assert signal is None
    
    def test_extract_low_satisfaction_signal(self, selector):
        """Test extracting low_satisfaction signal."""
        failure = {
            "user_query": "Help me debug",
            "selected_skills": ["generic_help"],
            "user_feedback_score": 2,  # 2 stars
        }
        
        signal = selector._extract_signal(failure, SignalType.LOW_SATISFACTION)
        
        assert signal is not None
        assert signal.signal_type == SignalType.LOW_SATISFACTION
        assert signal.target_metrics["satisfaction"] == 4.0  # Target: 4+ stars
    
    def test_extract_low_satisfaction_above_threshold(self, selector):
        """Test satisfaction above threshold returns None."""
        failure = {
            "user_query": "Good task",
            "selected_skills": ["good_skill"],
            "user_feedback_score": 4,  # 4 stars (above 3 threshold)
        }
        
        signal = selector._extract_signal(failure, SignalType.LOW_SATISFACTION)
        assert signal is None
    
    def test_update_learnings_with_signal(self, selector, db_session):
        """Test updating learnings with LearningSignal object."""
        signal = LearningSignal(
            signal_type=SignalType.SLOW_EXECUTION,
            query_pattern="run tests",
            wrong_skills=["slow_test"],
            correct_skills=["fast_test"],
            target_metrics={"time_ms": 2000},
        )
        
        selector._update_learnings(signal)
        
        # Verify in database
        from api.models import SkillSelectionLearning
        learning = db_session.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "run tests"
        ).first()
        
        assert learning is not None
        assert learning.signal_type == "slow_execution"
        assert learning.target_metrics["time_ms"] == 2000
        assert learning.confidence == 10.0
    
    def test_update_learnings_aggregates_metrics(self, selector, db_session):
        """Test that repeated learnings aggregate target metrics."""
        signal1 = LearningSignal(
            signal_type=SignalType.HIGH_COST,
            query_pattern="expensive task",
            wrong_skills=["expensive_skill"],
            correct_skills=["cheap_skill"],
            target_metrics={"cost": 0.20},
        )
        
        signal2 = LearningSignal(
            signal_type=SignalType.HIGH_COST,
            query_pattern="expensive task",
            wrong_skills=["expensive_skill"],
            correct_skills=["cheap_skill"],
            target_metrics={"cost": 0.10},
        )
        
        selector._update_learnings(signal1)
        selector._update_learnings(signal2)
        
        # Verify aggregation
        from api.models import SkillSelectionLearning
        learning = db_session.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "expensive task"
        ).first()
        
        assert learning.evidence_count == 2
        assert learning.confidence == 20.0
        # Weighted average: evidence_count=2, so weights are 1/2 old + 1/2 new
        # First: 0.20, Second: (0.20 * 0.5 + 0.10 * 0.5) = 0.15
        assert 0.10 <= learning.target_metrics["cost"] <= 0.20


class TestMultiFactorScoring:
    """Test multi-factor scoring system."""
    
    @pytest.fixture
    def selector(self, db_session):
        """Create selector with default weights."""
        return SelfImprovingSelector(db_session, llm_client=None)
    
    def test_calculate_perfect_score(self, selector):
        """Test perfect score calculation."""
        event = {
            "selection_correctness": 1,
            "execution_time_ms": 300,  # Very fast = 100 points
            "execution_cost": 0.0,  # Free = 100 points
            "user_feedback_score": 5,  # 5 stars = 100 points
        }
        
        score = selector.calculate_multi_factor_score(event)
        
        # All dimensions perfect ≈ 100
        assert score >= 99.5
    
    def test_calculate_mixed_score(self, selector):
        """Test mixed score calculation."""
        event = {
            "selection_correctness": 1,  # 100 * 0.4 = 40
            "execution_time_ms": 3000,  # 90 * 0.3 = 27
            "execution_cost": 0.05,  # 90 * 0.2 = 18
            "user_feedback_score": 4,  # 75 * 0.1 = 7.5
        }
        
        score = selector.calculate_multi_factor_score(event)
        
        # 40 + 27 + 18 + 7.5 = 92.5
        assert 92 <= score <= 93
    
    def test_calculate_poor_score(self, selector):
        """Test poor score calculation."""
        event = {
            "selection_correctness": 0,  # 0 * 0.4 = 0
            "execution_time_ms": 30000,  # 0 * 0.3 = 0
            "execution_cost": 1.0,  # 0 * 0.2 = 0
            "user_feedback_score": 1,  # 0 * 0.1 = 0
        }
        
        score = selector.calculate_multi_factor_score(event)
        
        # All dimensions poor = 0
        assert score == 0.0
    
    def test_calculate_score_with_missing_data(self, selector):
        """Test score calculation with missing metrics."""
        event = {
            "selection_correctness": 1,  # 100 * 0.4 = 40
            # Missing time, cost, satisfaction - use defaults
        }
        
        score = selector.calculate_multi_factor_score(event)
        
        # 40 + 100*0.3 + 100*0.2 + 75*0.1 = 40 + 30 + 20 + 7.5 = 97.5
        assert 97 <= score <= 98


class TestLearningStats:
    """Test learning statistics with multi-dimensional breakdown."""
    
    @pytest.fixture
    def selector(self, db_session):
        """Create selector."""
        return SelfImprovingSelector(db_session, llm_client=None)
    
    def test_get_learning_stats_breakdown(self, selector, db_session):
        """Test learning stats with signal type breakdown."""
        # Clear existing learnings first
        from api.models import SkillSelectionLearning
        db_session.query(SkillSelectionLearning).delete()
        db_session.commit()
        
        # Add fresh test data
        from uuid_utils import uuid7
        learnings = [
            SkillSelectionLearning(
                learning_id=str(uuid7()),
                query_pattern="test1",
                wrong_skills=[],
                correct_skills=[],
                confidence=80.0,
                signal_type="wrong_skill",
                target_metrics={"accuracy": 1.0},
            ),
            SkillSelectionLearning(
                learning_id=str(uuid7()),
                query_pattern="test2",
                wrong_skills=[],
                correct_skills=[],
                confidence=60.0,
                signal_type="slow_execution",
                target_metrics={"time_ms": 1000},
            ),
            SkillSelectionLearning(
                learning_id=str(uuid7()),
                query_pattern="test3",
                wrong_skills=[],
                correct_skills=[],
                confidence=90.0,
                signal_type="high_cost",
                target_metrics={"cost": 0.05},
            ),
        ]
        
        for learning in learnings:
            db_session.add(learning)
        db_session.commit()
        
        stats = selector.get_learning_stats()
        
        assert stats["total_learnings"] == 3
        assert stats["high_confidence"] == 2  # 80 and 90
        assert stats["low_confidence"] == 1  # 60
        
        # Check breakdown by signal type
        assert stats["by_signal_type"]["wrong_skill"] == 1
        assert stats["by_signal_type"]["slow_execution"] == 1
        assert stats["by_signal_type"]["high_cost"] == 1
        assert stats["by_signal_type"]["low_satisfaction"] == 0
        
        # Check weights included
        assert "weights" in stats
        assert stats["weights"]["accuracy"] == 0.4


class TestSignalThresholds:
    """Test configurable signal thresholds."""
    
    def test_default_thresholds(self):
        """Test default threshold values."""
        from core.skills.learning_signals import SignalThresholds
        
        thresholds = SignalThresholds()
        assert thresholds.slow_execution_ms == 5000
        assert thresholds.high_cost_usd == 0.10
        assert thresholds.low_satisfaction == 3
    
    def test_custom_thresholds(self, db_session):
        """Test custom threshold configuration."""
        from core.skills.learning_signals import SignalThresholds
        
        thresholds = SignalThresholds(
            slow_execution_ms=10000,  # 10 seconds
            high_cost_usd=0.50,  # $0.50
            low_satisfaction=2,  # < 2 stars
        )
        
        selector = SelfImprovingSelector(db_session, llm_client=None, thresholds=thresholds)
        
        # Test slow execution with custom threshold
        failure = {
            "user_query": "Test",
            "selected_skills": ["slow_skill"],
            "execution_time_ms": 8000,  # 8 seconds
        }
        
        # Should NOT trigger with 10s threshold
        signal = selector._extract_signal(failure, SignalType.SLOW_EXECUTION)
        assert signal is None
        
        # Should trigger with 5s threshold (above 8s)
        failure["execution_time_ms"] = 12000
        signal = selector._extract_signal(failure, SignalType.SLOW_EXECUTION)
        assert signal is not None
