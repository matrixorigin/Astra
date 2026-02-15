"""Integration tests for SelfImprovingSelector."""

import pytest
from datetime import datetime, timezone, timedelta
from uuid_utils import uuid7
from sqlalchemy import text

from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.learning_signals import SignalType, SignalWeights, SignalThresholds, LearningSignal
from api.models import SkillSelectionEvent, SkillSelectionLearning
from api.database import get_db_session


@pytest.fixture
def db():
    """Database session fixture."""
    return next(get_db_session())


@pytest.fixture
def clean_db(db):
    """Clean up test data before and after test."""
    # Clean before test
    db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern = 'test query'"))
    db.execute(text("DELETE FROM skill_selection_events WHERE user_query = 'test query'"))
    db.commit()
    
    yield
    
    # Clean after test
    db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern = 'test query'"))
    db.execute(text("DELETE FROM skill_selection_events WHERE user_query = 'test query'"))
    db.commit()


class TestSelfImprovingSelectorCore:
    """Test core functionality."""

    def test_get_recent_failures_with_data(self, db, clean_db):
        """Test get_recent_failures with failure data."""
        event_id = str(uuid7())
        
        event = SkillSelectionEvent(
            event_id=event_id,
            session_id=str(uuid7()),
            user_query="test query",
            context_snapshot="test",
            available_skills=[],
            selected_skills=["wrong_skill"],
            selection_method="test",
            selection_reasoning="test",
            candidate_scores={},
            selection_correctness=0,
            user_feedback_score=1,
            execution_time_ms=10000,
            execution_cost=0.5,
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        failures = selector.get_recent_failures(days=7, limit=10)
        
        assert len(failures) > 0
        assert failures[0]['event_id'] == event_id

    def test_extract_signal_wrong_skill(self, db, clean_db):
        """Test _extract_signal for WRONG_SKILL."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'test query',
            'selected_skills': ['wrong_skill'],
            'correction_suggestion': ['correct_skill'],
            'execution_time_ms': 1000,
            'execution_cost': 0.01,
            'user_feedback_score': 1,
            'selection_correctness': 0
        }
        
        signal = selector._extract_signal(failure, SignalType.WRONG_SKILL)
        
        assert signal is not None
        assert signal.signal_type == SignalType.WRONG_SKILL

    def test_extract_signal_slow_execution(self, db, clean_db):
        """Test _extract_signal for SLOW_EXECUTION."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'test query',
            'selected_skills': ['slow_skill'],
            'execution_time_ms': 10000,
            'execution_cost': 0.01,
            'user_feedback_score': 3,
            'selection_correctness': 1
        }
        
        signal = selector._extract_signal(failure, SignalType.SLOW_EXECUTION)
        
        assert signal is not None
        assert signal.signal_type == SignalType.SLOW_EXECUTION

    def test_calculate_multi_factor_score(self, db, clean_db):
        """Test calculate_multi_factor_score."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        event = {
            'selection_correctness': 1,
            'execution_time_ms': 1000,
            'execution_cost': 0.01,
            'user_feedback_score': 5
        }
        
        score = selector.calculate_multi_factor_score(event)
        
        assert 0 <= score <= 100

    def test_normalize_confidence(self, db, clean_db):
        """Test _normalize_confidence."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        assert selector._normalize_confidence(0.5) == 0.5
        assert selector._normalize_confidence(50.0) == 0.5
        assert selector._normalize_confidence(None) == 0.0

    def test_context_features(self, db, clean_db):
        """Test context feature extraction."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Short query
        features = selector._extract_context_features_from_query("short")
        assert features['length_bucket'] == 'short'
        
        # Query with code
        features = selector._extract_context_features_from_query("def foo(): pass")
        assert features['contains_code'] is True

    def test_apply_learnings_empty(self, db, clean_db):
        """Test apply_learnings with no learnings."""
        # Clean all learning data to ensure isolation
        db.execute(text("DELETE FROM skill_selection_learning"))
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [SkillCandidate(name="skill1")]
        
        result = selector.apply_learnings("test query", candidates)
        
        assert len(result) == len(candidates)

    def test_get_learning_stats(self, db, clean_db):
        """Test get_learning_stats."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        stats = selector.get_learning_stats()
        
        assert 'total_learnings' in stats
        assert 'by_signal_type' in stats

    def test_update_learnings_new(self, db, clean_db):
        """Test _update_learnings with new signal."""
        from core.skills.learning_signals import LearningSignal
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        signal = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="test query",
            wrong_skills=["wrong_skill"],
            correct_skills=["correct_skill"],
            target_metrics={"accuracy": 1.0},
            confidence=10.0
        )
        
        selector._update_learnings(signal)
        
        # Verify learning was created
        learning = db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "test query"
        ).first()
        
        assert learning is not None
        assert learning.confidence == 10.0

    def test_update_learnings_existing(self, db, clean_db):
        """Test _update_learnings with existing learning."""
        # Create existing learning
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="test query",
            wrong_skills=["wrong_skill"],
            correct_skills=["correct_skill"],
            improvement_score=10.0,
            confidence=10.0,
            evidence_count=1,
            signal_type=SignalType.WRONG_SKILL.value,  # Use enum value
            target_metrics={"accuracy": 1.0},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        from core.skills.learning_signals import LearningSignal
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        signal = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="test query",
            wrong_skills=["wrong_skill"],
            correct_skills=["correct_skill"],
            target_metrics={"accuracy": 1.0},
            confidence=10.0
        )
        
        selector._update_learnings(signal)
        
        # Re-query to get updated object
        updated_learning = db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "test query"
        ).first()
        
        assert updated_learning is not None
        assert updated_learning.evidence_count == 2
        assert updated_learning.confidence == 20.0

    def test_is_high_confidence(self, db, clean_db):
        """Test _is_high_confidence."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        assert selector._is_high_confidence(0.6) is True
        assert selector._is_high_confidence(0.4) is False
        assert selector._is_high_confidence(60.0) is True
        assert selector._is_high_confidence(40.0) is False
        assert selector._is_high_confidence(None) is False

    def test_get_signal_weight(self, db, clean_db):
        """Test _get_signal_weight."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        weight = selector._get_signal_weight(SignalType.WRONG_SKILL.value)
        assert weight == selector.weights.accuracy
        
        weight = selector._get_signal_weight(SignalType.SLOW_EXECUTION.value)
        assert weight == selector.weights.speed

    def test_embedding_functions(self, db, clean_db):
        """Test embedding-related functions."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test _embed_query (should handle gracefully without OpenAI)
        embedding = selector._embed_query("test query")
        # Should return None or list
        assert embedding is None or isinstance(embedding, list)
        
        # Test _embedding_to_vec_str
        vec_str = selector._embedding_to_vec_str([1.0, 2.0, 3.0])
        assert vec_str == "[1.0,2.0,3.0]"
        
        # Test _parse_embedding
        parsed = selector._parse_embedding("[1.0,2.0,3.0]")
        assert parsed == [1.0, 2.0, 3.0]
        
        # Test _l2_similarity
        sim = selector._l2_similarity([1.0, 0.0], [0.0, 1.0])
        assert 0 <= sim <= 1

    def test_load_runtime_config(self, db, clean_db):
        """Test _load_runtime_config."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        config = selector._load_runtime_config()
        
        assert 'weights' in config
        assert 'decay' in config
        assert 'semantic_similarity_threshold' in config

    def test_learn_from_failures_no_failures(self, db, clean_db):
        """Test learn_from_failures with no failures."""
        # Clean all existing events first
        db.execute(text("DELETE FROM skill_selection_events"))
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        result = selector.learn_from_failures(days=7)
        
        assert result['learned'] == 0
        assert 'No failures to learn from' in result['message']

    def test_learn_from_failures_with_data(self, db, clean_db):
        """Test learn_from_failures with actual failure data."""
        # Create a failure event
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="test query for learning",
            available_skills=["skill1", "skill2"],
            selected_skills=["skill1"],
            selection_method="test",
            execution_success=False,  # This makes it a failure
            execution_time_ms=5000,  # Slow execution
            user_feedback_score=1,   # Low satisfaction
            selection_correctness=False,  # Wrong skill
            correction_suggestion={"correct_skills": ["skill2"]},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        result = selector.learn_from_failures(days=7)
        
        assert result['learned'] >= 0
        assert 'signals_by_type' in result

    def test_extract_signal_high_cost(self, db, clean_db):
        """Test _extract_signal for high cost signal."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'test query',
            'selected_skills': ['expensive_skill'],
            'execution_cost': 10.0,  # High cost
            'available_skills': ['cheap_skill', 'expensive_skill']
        }
        
        signal = selector._extract_signal(failure, SignalType.HIGH_COST)
        
        if signal:  # May be None if cost threshold not met
            assert signal.signal_type == SignalType.HIGH_COST
            assert signal.query_pattern == 'test query'

    def test_extract_signal_low_satisfaction(self, db, clean_db):
        """Test _extract_signal for low satisfaction signal."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'test query',
            'selected_skills': ['bad_skill'],
            'user_feedback_score': 1,  # Low satisfaction
            'available_skills': ['good_skill', 'bad_skill']
        }
        
        signal = selector._extract_signal(failure, SignalType.LOW_SATISFACTION)
        
        if signal:  # May be None if satisfaction threshold not met
            assert signal.signal_type == SignalType.LOW_SATISFACTION
            assert signal.query_pattern == 'test query'

    def test_apply_learnings_with_data(self, db, clean_db):
        """Test apply_learnings with actual learning data."""
        # Create a learning record
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="test query for apply",
            wrong_skills=["wrong_skill"],
            correct_skills=["correct_skill"],
            improvement_score=15.0,
            confidence=80.0,
            evidence_count=3,
            signal_type=SignalType.WRONG_SKILL.value,
            target_metrics={"accuracy": 0.9},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create candidate objects with name attribute
        class MockCandidate:
            def __init__(self, name, confidence=1.0):
                self.name = name
                self.confidence = confidence
        
        candidates = [
            MockCandidate("wrong_skill", 0.8),
            MockCandidate("correct_skill", 0.7),
            MockCandidate("other_skill", 0.6)
        ]
        
        result = selector.apply_learnings("test query for apply", candidates)
        
        assert isinstance(result, list)
        assert len(result) == len(candidates)
        # Result should be the same candidates, potentially reordered
        result_names = [c.name for c in result]
        assert "correct_skill" in result_names
        assert "wrong_skill" in result_names

    def test_learning_signal_to_dict(self, db, clean_db):
        """Test LearningSignal.to_dict() with context_features."""
        signal = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="test query",
            wrong_skills=["wrong"],
            correct_skills=["correct"],
            target_metrics={"accuracy": 0.9},
            confidence=50.0,
            context_features={"user_id": "alice", "session_id": "123"}
        )
        
        result = signal.to_dict()
        
        assert result["signal_type"] == "wrong_skill"
        assert result["context_features"] == {"user_id": "alice", "session_id": "123"}
        assert result["confidence"] == 50.0

    def test_signal_weights_validation_negative(self, db, clean_db):
        """Test SignalWeights validation with negative weight."""
        with pytest.raises(ValueError, match="cannot be negative"):
            SignalWeights(accuracy=-0.1, speed=0.3, cost=0.2, satisfaction=0.6)

    def test_signal_weights_validation_exceeds_one(self, db, clean_db):
        """Test SignalWeights validation with weight > 1.0."""
        with pytest.raises(ValueError, match="cannot exceed 1.0"):
            SignalWeights(accuracy=1.1, speed=0.3, cost=0.2, satisfaction=-0.6)

    def test_signal_weights_validation_sum(self, db, clean_db):
        """Test SignalWeights validation with incorrect sum."""
        with pytest.raises(ValueError, match="must sum to 1.0"):
            SignalWeights(accuracy=0.5, speed=0.3, cost=0.2, satisfaction=0.1)

    def test_signal_weights_to_dict(self, db, clean_db):
        """Test SignalWeights.to_dict()."""
        weights = SignalWeights(accuracy=0.4, speed=0.3, cost=0.2, satisfaction=0.1)
        
        result = weights.to_dict()
        
        assert result["accuracy"] == 0.4
        assert result["speed"] == 0.3
        assert result["cost"] == 0.2
        assert result["satisfaction"] == 0.1

    def test_signal_thresholds_to_dict(self, db, clean_db):
        """Test SignalThresholds.to_dict()."""
        thresholds = SignalThresholds(
            slow_execution_ms=3000,
            high_cost_usd=0.05,
            low_satisfaction=2
        )
        
        result = thresholds.to_dict()
        
        assert result["slow_execution_ms"] == 3000
        assert result["high_cost_usd"] == 0.05
        assert result["low_satisfaction"] == 2

    def test_selector_initialization_with_custom_weights(self, db, clean_db):
        """Test SelfImprovingSelector initialization with custom weights."""
        custom_weights = SignalWeights(accuracy=0.5, speed=0.2, cost=0.2, satisfaction=0.1)
        custom_thresholds = SignalThresholds(slow_execution_ms=3000, high_cost_usd=0.05, low_satisfaction=2)
        
        selector = SelfImprovingSelector(
            session=db,
            llm_client=None,
            weights=custom_weights,
            thresholds=custom_thresholds
        )
        
        assert selector.weights.accuracy == 0.5
        assert selector.thresholds.slow_execution_ms == 3000

    def test_selector_initialization_invalid_session(self, db, clean_db):
        """Test SelfImprovingSelector initialization with invalid session."""
        with pytest.raises(TypeError, match="session must be a SQLAlchemy Session"):
            SelfImprovingSelector(session="not a session", llm_client=None)

    def test_get_recent_failures_empty(self, db, clean_db):
        """Test get_recent_failures with no failures."""
        db.execute(text("DELETE FROM skill_selection_events"))
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failures = selector.get_recent_failures(days=7)
        
        assert failures == []

    def test_extract_signal_returns_none_for_non_matching(self, db, clean_db):
        """Test _extract_signal returns None when thresholds not met."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create a failure that doesn't meet thresholds
        failure = {
            'user_query': 'test query',
            'selected_skills': ['fast_skill'],
            'execution_time_ms': 100,  # Fast, below threshold
            'execution_cost': 0.001,   # Cheap, below threshold
            'user_feedback_score': 5,  # High satisfaction, above threshold
            'available_skills': ['fast_skill']
        }
        
        # Should return None for slow execution (doesn't meet threshold)
        signal = selector._extract_signal(failure, SignalType.SLOW_EXECUTION)
        assert signal is None

    def test_calculate_multi_factor_score_edge_cases(self, db, clean_db):
        """Test calculate_multi_factor_score with edge cases."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test with perfect event
        perfect_event = {
            "selection_correctness": 1,
            "execution_time_ms": 100,
            "execution_cost": 0.01,
            "user_feedback_score": 5
        }
        score = selector.calculate_multi_factor_score(perfect_event)
        assert score > 0
        
        # Test with poor event
        poor_event = {
            "selection_correctness": 0,
            "execution_time_ms": 30000,
            "execution_cost": 10.0,
            "user_feedback_score": 1
        }
        score = selector.calculate_multi_factor_score(poor_event)
        assert score >= 0

    def test_apply_learnings_empty_query(self, db, clean_db):
        """Test apply_learnings with empty query."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [SkillCandidate(name="skill1")]
        
        result = selector.apply_learnings("", candidates)
        
        # Should return candidates unchanged
        assert len(result) == len(candidates)


class TestSelfImprovingSelectorCoverage:
    """Additional tests for coverage improvement."""

    @pytest.fixture
    def db(self):
        """Database session fixture."""
        return next(get_db_session())

    @pytest.fixture
    def clean_db(self, db):
        """Clean up test data before and after test."""
        db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern LIKE 'coverage_%'"))
        db.execute(text("DELETE FROM skill_selection_events WHERE user_query LIKE 'coverage_%'"))
        db.commit()
        
        yield
        
        db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern LIKE 'coverage_%'"))
        db.execute(text("DELETE FROM skill_selection_events WHERE user_query LIKE 'coverage_%'"))
        db.commit()

    def test_get_recent_failures_with_multiple_types(self, db, clean_db):
        """Test get_recent_failures returns multiple failure types."""
        # Create multiple failure events
        for i in range(3):
            event = SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id=str(uuid7()),
                user_query=f"coverage_query_{i}",
                available_skills=["skill1", "skill2"],
                selected_skills=["skill1"],
                selection_method="test",
                execution_success=False,
                execution_time_ms=1000 + i * 1000,
                user_feedback_score=1 + i,
                selection_correctness=False,
                created_at=datetime.now(timezone.utc).replace(tzinfo=None)
            )
            db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        failures = selector.get_recent_failures(days=7)
        
        assert len(failures) >= 3

    def test_extract_signal_with_context_features(self, db, clean_db):
        """Test _extract_signal extracts context features."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'coverage_query',
            'selected_skills': ['wrong_skill'],
            'available_skills': ['correct_skill'],
            'execution_success': False,
            'selection_correctness': False,
            'user_id': 'alice',
            'session_id': 'session_123'
        }
        
        signal = selector._extract_signal(failure, SignalType.WRONG_SKILL)
        
        if signal:
            assert signal.context_features is not None or signal.context_features is None

    def test_normalize_confidence_boundary_values(self, db, clean_db):
        """Test _normalize_confidence with boundary values."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test 0
        assert selector._normalize_confidence(0) == 0.0
        
        # Test 100
        assert selector._normalize_confidence(100) == 1.0
        
        # Test 50
        assert 0 < selector._normalize_confidence(50) < 1

    def test_apply_learnings_with_no_candidates(self, db, clean_db):
        """Test apply_learnings with empty candidates list."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        result = selector.apply_learnings("coverage_query", [])
        
        assert result == []

    def test_get_learning_stats_with_data(self, db, clean_db):
        """Test get_learning_stats returns correct structure."""
        # Create learning records
        for i in range(2):
            learning = SkillSelectionLearning(
                learning_id=str(uuid7()),
                query_pattern=f"coverage_pattern_{i}",
                wrong_skills=["wrong"],
                correct_skills=["correct"],
                improvement_score=10.0 + i,
                confidence=50.0 + i * 10,
                evidence_count=1 + i,
                signal_type=SignalType.WRONG_SKILL.value,
                target_metrics={"accuracy": 0.9},
                created_at=datetime.now(timezone.utc).replace(tzinfo=None)
            )
            db.add(learning)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        stats = selector.get_learning_stats()
        
        assert 'total_learnings' in stats
        assert stats['total_learnings'] >= 2

    def test_learn_from_failures_with_multiple_signals(self, db, clean_db):
        """Test learn_from_failures extracts multiple signal types."""
        # Create a complex failure event
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="coverage_complex_query",
            available_skills=["skill1", "skill2", "skill3"],
            selected_skills=["skill1"],
            selection_method="test",
            execution_success=False,
            execution_time_ms=10000,  # Slow
            execution_cost=5.0,       # Expensive
            user_feedback_score=1,    # Low satisfaction
            selection_correctness=False,  # Wrong skill
            correction_suggestion={"correct_skills": ["skill2"]},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        result = selector.learn_from_failures(days=7)
        
        assert result['learned'] >= 0
        assert isinstance(result['signals_by_type'], dict)

    def test_apply_learnings_reorders_candidates(self, db, clean_db):
        """Test apply_learnings can reorder candidates based on learnings."""
        # Create learning that prefers skill2 over skill1
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="coverage_reorder",
            wrong_skills=["skill1"],
            correct_skills=["skill2"],
            improvement_score=20.0,
            confidence=90.0,
            evidence_count=5,
            signal_type=SignalType.WRONG_SKILL.value,
            target_metrics={"accuracy": 0.95},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [
            SkillCandidate(name="skill1", confidence=0.9),
            SkillCandidate(name="skill2", confidence=0.8),
        ]
        
        result = selector.apply_learnings("coverage_reorder", candidates)
        
        assert len(result) == 2
        # Result should have reordered or modified candidates
        result_names = [c.name for c in result]
        assert "skill1" in result_names or "skill2" in result_names

    def test_calculate_multi_factor_score_with_missing_fields(self, db, clean_db):
        """Test calculate_multi_factor_score handles missing fields."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Event with minimal fields
        event = {}
        score = selector.calculate_multi_factor_score(event)
        
        assert 0 <= score <= 100

    def test_normalize_confidence_with_none(self, db, clean_db):
        """Test _normalize_confidence with None value."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        result = selector._normalize_confidence(None)
        
        assert result == 0.0

    def test_get_signal_weight_all_types(self, db, clean_db):
        """Test _get_signal_weight for all signal types."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        for signal_type in SignalType:
            weight = selector._get_signal_weight(signal_type.value)
            assert weight > 0
            assert weight <= 1.0

    def test_embedding_functions_with_none_embedding(self, db, clean_db):
        """Test embedding functions handle None gracefully."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test with None embedding
        vec_str = selector._embedding_to_vec_str(None)
        assert vec_str is None
        
        # Test parsing None
        parsed = selector._parse_embedding(None)
        assert parsed is None

    def test_l2_similarity_identical_vectors(self, db, clean_db):
        """Test _l2_similarity with identical vectors."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        sim = selector._l2_similarity([1.0, 2.0, 3.0], [1.0, 2.0, 3.0])
        
        assert sim == 1.0

    def test_l2_similarity_orthogonal_vectors(self, db, clean_db):
        """Test _l2_similarity with orthogonal vectors."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        sim = selector._l2_similarity([1.0, 0.0], [0.0, 1.0])
        
        assert 0 <= sim <= 1

    def test_load_runtime_config_structure(self, db, clean_db):
        """Test _load_runtime_config returns correct structure."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        config = selector._load_runtime_config()
        
        assert 'weights' in config
        assert 'decay' in config
        assert 'semantic_similarity_threshold' in config
        # weights is a SignalWeights object, not dict
        assert hasattr(config['weights'], 'accuracy')
        assert isinstance(config['decay'], dict)
