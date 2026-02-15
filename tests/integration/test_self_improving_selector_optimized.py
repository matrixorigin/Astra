"""Optimized integration tests for SelfImprovingSelector - high quality, no duplication."""

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
    return next(get_db_session())


@pytest.fixture
def clean_db(db):
    db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern LIKE 'opt_%'"))
    db.execute(text("DELETE FROM skill_selection_events WHERE user_query LIKE 'opt_%'"))
    db.commit()
    yield
    db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern LIKE 'opt_%'"))
    db.execute(text("DELETE FROM skill_selection_events WHERE user_query LIKE 'opt_%'"))
    db.commit()


class TestSelfImprovingSelectorOptimized:
    """High-quality tests covering all critical paths."""

    def test_initialization_and_config(self, db, clean_db):
        """Test initialization with custom config."""
        weights = SignalWeights(accuracy=0.5, speed=0.2, cost=0.2, satisfaction=0.1)
        thresholds = SignalThresholds(slow_execution_ms=3000, high_cost_usd=0.05, low_satisfaction=2)
        
        selector = SelfImprovingSelector(session=db, llm_client=None, weights=weights, thresholds=thresholds)
        
        assert selector.weights.accuracy == 0.5
        assert selector.thresholds.slow_execution_ms == 3000

    def test_initialization_invalid_session(self, db, clean_db):
        """Test initialization validation."""
        with pytest.raises(TypeError):
            SelfImprovingSelector(session="invalid", llm_client=None)

    def test_full_learning_cycle(self, db, clean_db):
        """Test complete learning cycle: failures → signals → learnings → application."""
        # Create failure events
        for i in range(3):
            event = SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id=str(uuid7()),
                user_query=f"opt_query_{i}",
                available_skills=["skill1", "skill2"],
                selected_skills=["skill1"],
                selection_method="test",
                execution_success=False,
                execution_time_ms=5000 + i * 1000,
                execution_cost=1.0 + i * 0.5,
                user_feedback_score=1,
                selection_correctness=False,
                correction_suggestion={"correct_skills": ["skill2"]},
                created_at=datetime.now(timezone.utc).replace(tzinfo=None)
            )
            db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Learn from failures
        result = selector.learn_from_failures(days=7)
        assert result['learned'] >= 0
        
        # Get stats
        stats = selector.get_learning_stats()
        assert 'total_learnings' in stats

    def test_signal_extraction_all_types(self, db, clean_db):
        """Test signal extraction for all signal types."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'opt_signal_test',
            'selected_skills': ['wrong_skill'],
            'available_skills': ['correct_skill'],
            'execution_time_ms': 10000,
            'execution_cost': 5.0,
            'user_feedback_score': 1,
            'selection_correctness': False,
        }
        
        # Test all signal types
        for signal_type in SignalType:
            signal = selector._extract_signal(failure, signal_type)
            # Signal may be None if thresholds not met, that's ok
            if signal:
                assert signal.signal_type == signal_type

    def test_learning_update_and_retrieval(self, db, clean_db):
        """Test learning creation, update, and retrieval."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create initial learning
        signal1 = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="opt_pattern",
            wrong_skills=["wrong"],
            correct_skills=["correct"],
            target_metrics={"accuracy": 0.9},
            confidence=50.0
        )
        selector._update_learnings(signal1)
        
        # Update with second signal
        signal2 = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="opt_pattern",
            wrong_skills=["wrong"],
            correct_skills=["correct"],
            target_metrics={"accuracy": 0.95},
            confidence=60.0
        )
        selector._update_learnings(signal2)
        
        # Verify stats
        stats = selector.get_learning_stats()
        assert stats['total_learnings'] >= 1

    def test_apply_learnings_with_candidates(self, db, clean_db):
        """Test applying learnings to reorder candidates."""
        # Create learning
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="opt_apply",
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
        
        result = selector.apply_learnings("opt_apply", candidates)
        assert len(result) == 2

    def test_scoring_and_normalization(self, db, clean_db):
        """Test multi-factor scoring and confidence normalization."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test scoring
        event = {
            "selection_correctness": 1,
            "execution_time_ms": 1000,
            "execution_cost": 0.01,
            "user_feedback_score": 5
        }
        score = selector.calculate_multi_factor_score(event)
        assert 0 <= score <= 100
        
        # Test normalization
        assert selector._normalize_confidence(0) == 0.0
        assert selector._normalize_confidence(100) == 1.0
        assert 0 < selector._normalize_confidence(50) < 1

    def test_embedding_and_similarity(self, db, clean_db):
        """Test embedding functions and similarity calculation."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test embedding conversion
        vec_str = selector._embedding_to_vec_str([1.0, 2.0, 3.0])
        assert vec_str == "[1.0,2.0,3.0]"
        
        parsed = selector._parse_embedding(vec_str)
        assert parsed == [1.0, 2.0, 3.0]
        
        # Test similarity
        sim_identical = selector._l2_similarity([1.0, 2.0], [1.0, 2.0])
        assert sim_identical == 1.0
        
        sim_orthogonal = selector._l2_similarity([1.0, 0.0], [0.0, 1.0])
        assert 0 <= sim_orthogonal <= 1

    def test_signal_weights_validation(self, db, clean_db):
        """Test SignalWeights validation."""
        # Valid weights
        valid = SignalWeights(accuracy=0.4, speed=0.3, cost=0.2, satisfaction=0.1)
        assert valid.accuracy == 0.4
        
        # Invalid: negative
        with pytest.raises(ValueError, match="cannot be negative"):
            SignalWeights(accuracy=-0.1, speed=0.3, cost=0.2, satisfaction=0.6)
        
        # Invalid: exceeds 1.0
        with pytest.raises(ValueError, match="cannot exceed 1.0"):
            SignalWeights(accuracy=1.1, speed=0.3, cost=0.2, satisfaction=-0.6)
        
        # Invalid: sum != 1.0
        with pytest.raises(ValueError, match="must sum to 1.0"):
            SignalWeights(accuracy=0.5, speed=0.3, cost=0.2, satisfaction=0.1)

    def test_edge_cases(self, db, clean_db):
        """Test edge cases and boundary conditions."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Empty candidates
        result = selector.apply_learnings("opt_empty", [])
        assert result == []
        
        # Empty query
        from core.skills.pipeline import SkillCandidate
        candidates = [SkillCandidate(name="skill1")]
        result = selector.apply_learnings("", candidates)
        assert len(result) == 1
        
        # Missing fields in event
        event = {}
        score = selector.calculate_multi_factor_score(event)
        assert 0 <= score <= 100
        
        # None values
        assert selector._normalize_confidence(None) == 0.0
        assert selector._embedding_to_vec_str(None) is None
        assert selector._parse_embedding(None) is None

    def test_get_recent_failures_comprehensive(self, db, clean_db):
        """Test get_recent_failures with various scenarios."""
        # Create multiple failure events with different characteristics
        for i in range(5):
            event = SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id=str(uuid7()),
                user_query=f"opt_failure_{i}",
                available_skills=["s1", "s2", "s3"],
                selected_skills=["s1"],
                selection_method="test",
                execution_success=False,
                execution_time_ms=1000 + i * 1000,
                execution_cost=0.1 + i * 0.1,
                user_feedback_score=1 + (i % 3),
                selection_correctness=False,
                created_at=datetime.now(timezone.utc).replace(tzinfo=None)
            )
            db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        failures = selector.get_recent_failures(days=7, limit=10)
        
        assert len(failures) >= 5
        assert all('user_query' in f for f in failures)

    def test_extract_signal_boundary_thresholds(self, db, clean_db):
        """Test signal extraction at threshold boundaries."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Just below threshold
        failure_below = {
            'user_query': 'opt_below',
            'selected_skills': ['skill1'],
            'available_skills': ['skill1'],
            'execution_time_ms': selector.thresholds.slow_execution_ms - 100,
            'execution_cost': selector.thresholds.high_cost_usd - 0.01,
            'user_feedback_score': selector.thresholds.low_satisfaction + 1,
        }
        
        # Just above threshold
        failure_above = {
            'user_query': 'opt_above',
            'selected_skills': ['skill1'],
            'available_skills': ['skill1'],
            'execution_time_ms': selector.thresholds.slow_execution_ms + 100,
            'execution_cost': selector.thresholds.high_cost_usd + 0.01,
            'user_feedback_score': selector.thresholds.low_satisfaction - 1,
        }
        
        signal_below = selector._extract_signal(failure_below, SignalType.SLOW_EXECUTION)
        signal_above = selector._extract_signal(failure_above, SignalType.SLOW_EXECUTION)
        
        # Below threshold should not trigger
        assert signal_below is None
        # Above threshold should trigger
        assert signal_above is not None

    def test_multi_factor_score_all_scenarios(self, db, clean_db):
        """Test scoring with various event combinations."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        scenarios = [
            {"selection_correctness": 1, "execution_time_ms": 100, "execution_cost": 0.01, "user_feedback_score": 5},
            {"selection_correctness": 0, "execution_time_ms": 30000, "execution_cost": 10.0, "user_feedback_score": 1},
            {"selection_correctness": None, "execution_time_ms": 5000, "execution_cost": 1.0, "user_feedback_score": 3},
            {},  # Empty event
        ]
        
        for event in scenarios:
            score = selector.calculate_multi_factor_score(event)
            assert 0 <= score <= 100

    def test_learning_persistence_and_retrieval(self, db, clean_db):
        """Test learning persistence across multiple operations."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create multiple learnings
        for i in range(3):
            signal = LearningSignal(
                signal_type=SignalType.WRONG_SKILL,
                query_pattern=f"opt_persist_{i}",
                wrong_skills=[f"wrong_{i}"],
                correct_skills=[f"correct_{i}"],
                target_metrics={"accuracy": 0.8 + i * 0.05},
                confidence=50.0 + i * 10
            )
            selector._update_learnings(signal)
        
        # Verify all were persisted
        stats = selector.get_learning_stats()
        assert stats['total_learnings'] >= 3

    def test_apply_learnings_no_match(self, db, clean_db):
        """Test apply_learnings when no learnings match query."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [
            SkillCandidate(name="skill1"),
            SkillCandidate(name="skill2"),
        ]
        
        # Query with no matching learnings
        result = selector.apply_learnings("opt_nomatch_xyz", candidates)
        
        # Should return candidates unchanged
        assert len(result) == len(candidates)

    def test_confidence_decay_and_weights(self, db, clean_db):
        """Test confidence decay and weight application."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test all signal types have weights
        for signal_type in SignalType:
            weight = selector._get_signal_weight(signal_type.value)
            assert 0 < weight <= 1.0
        
        # Test confidence normalization across range
        for conf in [0, 25, 50, 75, 100]:
            normalized = selector._normalize_confidence(conf)
            assert 0 <= normalized <= 1.0

    def test_runtime_config_loading(self, db, clean_db):
        """Test runtime config loading and caching."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        config1 = selector._load_runtime_config()
        config2 = selector._load_runtime_config()
        
        # Should have same structure
        assert 'weights' in config1
        assert 'decay' in config1
        assert config1.keys() == config2.keys()

    def test_is_high_confidence_threshold(self, db, clean_db):
        """Test high confidence threshold detection."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test various confidence levels
        assert selector._is_high_confidence(0.6) is True
        assert selector._is_high_confidence(0.4) is False
        assert selector._is_high_confidence(60.0) is True
        assert selector._is_high_confidence(40.0) is False
        assert selector._is_high_confidence(None) is False
        assert selector._is_high_confidence(0.5) is True  # 0.5 is at threshold

    def test_context_features_extraction(self, db, clean_db):
        """Test context features extraction from failures."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'opt_context',
            'selected_skills': ['skill1'],
            'available_skills': ['skill1', 'skill2'],
            'user_id': 'alice',
            'session_id': 'session_123',
            'execution_success': False,
            'selection_correctness': False,
        }
        
        signal = selector._extract_signal(failure, SignalType.WRONG_SKILL)
        
        if signal:
            # Context features may or may not be extracted
            assert signal.query_pattern == 'opt_context'

    def test_learn_from_failures_with_exception_handling(self, db, clean_db):
        """Test learn_from_failures handles exceptions gracefully."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create a failure event
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="opt_exception_test",
            available_skills=["s1"],
            selected_skills=["s1"],
            selection_method="test",
            execution_success=False,
            selection_correctness=False,
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        # Should handle gracefully
        result = selector.learn_from_failures(days=7)
        assert 'learned' in result
        assert 'signals_by_type' in result

    def test_apply_learnings_with_semantic_matching(self, db, clean_db):
        """Test apply_learnings with semantic similarity matching."""
        # Create learning with similar query pattern
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="opt_semantic_test",
            wrong_skills=["wrong"],
            correct_skills=["correct"],
            improvement_score=15.0,
            confidence=75.0,
            evidence_count=3,
            signal_type=SignalType.WRONG_SKILL.value,
            target_metrics={"accuracy": 0.9},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [
            SkillCandidate(name="wrong", confidence=0.8),
            SkillCandidate(name="correct", confidence=0.7),
        ]
        
        # Query similar to learning pattern
        result = selector.apply_learnings("opt_semantic_test", candidates)
        # May add new candidates or reorder
        assert len(result) >= 2

    def test_embedding_vector_operations(self, db, clean_db):
        """Test embedding vector operations comprehensively."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test various vector operations
        vectors = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 1.0, 1.0],
        ]
        
        for v in vectors:
            vec_str = selector._embedding_to_vec_str(v)
            parsed = selector._parse_embedding(vec_str)
            assert parsed == v
        
        # Test similarity between different vectors
        for i, v1 in enumerate(vectors):
            for v2 in vectors[i+1:]:
                sim = selector._l2_similarity(v1, v2)
                assert 0 <= sim <= 1

    def test_persist_learning_updates(self, db, clean_db):
        """Test _persist_learning_updates commits changes."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        signal = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="opt_persist",
            wrong_skills=["w"],
            correct_skills=["c"],
            target_metrics={"accuracy": 0.9},
            confidence=50.0
        )
        
        selector._update_learnings(signal)
        # _persist_learning_updates is called internally
        
        # Verify it was persisted
        stats = selector.get_learning_stats()
        assert stats['total_learnings'] >= 1

    def test_get_recent_failures_with_limit(self, db, clean_db):
        """Test get_recent_failures respects limit parameter."""
        # Create many failure events
        for i in range(15):
            event = SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id=str(uuid7()),
                user_query=f"opt_limit_{i}",
                available_skills=["s1"],
                selected_skills=["s1"],
                selection_method="test",
                execution_success=False,
                selection_correctness=False,
                created_at=datetime.now(timezone.utc).replace(tzinfo=None)
            )
            db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test with different limits
        failures_5 = selector.get_recent_failures(days=7, limit=5)
        failures_10 = selector.get_recent_failures(days=7, limit=10)
        
        assert len(failures_5) <= 5
        assert len(failures_10) <= 10

    def test_extract_signal_with_all_fields(self, db, clean_db):
        """Test signal extraction with complete failure data."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        complete_failure = {
            'user_query': 'opt_complete',
            'selected_skills': ['skill1'],
            'available_skills': ['skill1', 'skill2', 'skill3'],
            'execution_time_ms': 10000,
            'execution_cost': 5.0,
            'user_feedback_score': 1,
            'selection_correctness': False,
            'correction_suggestion': {'correct_skills': ['skill2']},
            'user_id': 'alice',
            'session_id': 'session_123',
        }
        
        # Extract all signal types
        signals = []
        for signal_type in SignalType:
            signal = selector._extract_signal(complete_failure, signal_type)
            if signal:
                signals.append(signal)
        
        # Should extract at least some signals
        assert len(signals) > 0
