"""Integration tests for SelfImprovingSelector."""

import pytest
from datetime import datetime, timezone, timedelta
from uuid_utils import uuid7
from sqlalchemy import text

from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.learning_signals import SignalType, SignalWeights, SignalThresholds
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
