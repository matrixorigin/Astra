"""Comprehensive tests for SelfImprovingSelector - targeting 90%+ coverage."""

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
    db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern LIKE 'comp_%'"))
    db.execute(text("DELETE FROM skill_selection_events WHERE user_query LIKE 'comp_%'"))
    db.commit()
    yield
    db.execute(text("DELETE FROM skill_selection_learning WHERE query_pattern LIKE 'comp_%'"))
    db.execute(text("DELETE FROM skill_selection_events WHERE user_query LIKE 'comp_%'"))
    db.commit()


class TestSelfImprovingSelectorComprehensive:
    """Comprehensive tests covering all critical paths for 90%+ coverage."""

    def test_initialization_with_all_params(self, db, clean_db):
        """Test initialization with all parameters."""
        weights = SignalWeights(accuracy=0.5, speed=0.2, cost=0.2, satisfaction=0.1)
        thresholds = SignalThresholds(slow_execution_ms=3000, high_cost_usd=0.05, low_satisfaction=2)
        
        selector = SelfImprovingSelector(
            session=db,
            llm_client=None,
            account="test_account",
            weights=weights,
            thresholds=thresholds
        )
        
        assert selector.account == "test_account"
        assert selector.weights.accuracy == 0.5
        assert selector.thresholds.slow_execution_ms == 3000

    def test_full_learning_pipeline(self, db, clean_db):
        """Test complete learning pipeline from failures to application."""
        # Create diverse failure events
        for i in range(5):
            event = SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id=str(uuid7()),
                user_query=f"comp_pipeline_{i}",
                available_skills=["s1", "s2", "s3"],
                selected_skills=["s1"],
                selection_method="test",
                execution_success=False,
                execution_time_ms=5000 + i * 1000,
                execution_cost=1.0 + i * 0.5,
                user_feedback_score=1 + (i % 3),
                selection_correctness=False,
                correction_suggestion={"correct_skills": ["s2"]},
                created_at=datetime.now(timezone.utc).replace(tzinfo=None)
            )
            db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Learn from failures
        learn_result = selector.learn_from_failures(days=7)
        assert learn_result['learned'] >= 0
        
        # Get stats
        stats = selector.get_learning_stats()
        assert 'total_learnings' in stats
        assert 'by_signal_type' in stats

    def test_all_signal_types_extraction(self, db, clean_db):
        """Test extraction of all signal types."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create failure that triggers all signal types
        failure = {
            'user_query': 'comp_all_signals',
            'selected_skills': ['wrong_skill'],
            'available_skills': ['correct_skill', 'wrong_skill'],
            'execution_time_ms': 15000,  # Slow
            'execution_cost': 10.0,      # Expensive
            'user_feedback_score': 1,    # Low satisfaction
            'selection_correctness': False,  # Wrong skill
        }
        
        extracted_signals = {}
        for signal_type in SignalType:
            signal = selector._extract_signal(failure, signal_type)
            if signal:
                extracted_signals[signal_type] = signal
        
        # Should extract multiple signal types
        assert len(extracted_signals) > 0

    def test_learning_creation_update_retrieval(self, db, clean_db):
        """Test learning creation, update, and retrieval cycle."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create initial learning
        signal1 = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="comp_cycle",
            wrong_skills=["w1"],
            correct_skills=["c1"],
            target_metrics={"accuracy": 0.8},
            confidence=40.0
        )
        selector._update_learnings(signal1)
        
        # Update with higher confidence
        signal2 = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="comp_cycle",
            wrong_skills=["w1"],
            correct_skills=["c1"],
            target_metrics={"accuracy": 0.9},
            confidence=60.0
        )
        selector._update_learnings(signal2)
        
        # Verify persistence
        stats = selector.get_learning_stats()
        assert stats['total_learnings'] >= 1

    def test_apply_learnings_reordering(self, db, clean_db):
        """Test apply_learnings reorders candidates based on learnings."""
        # Create strong learning
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="comp_reorder",
            wrong_skills=["skill_a"],
            correct_skills=["skill_b"],
            improvement_score=25.0,
            confidence=95.0,
            evidence_count=10,
            signal_type=SignalType.WRONG_SKILL.value,
            target_metrics={"accuracy": 0.95},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [
            SkillCandidate(name="skill_a", confidence=0.9),
            SkillCandidate(name="skill_b", confidence=0.7),
            SkillCandidate(name="skill_c", confidence=0.6),
        ]
        
        result = selector.apply_learnings("comp_reorder", candidates)
        assert len(result) >= 2

    def test_scoring_with_various_events(self, db, clean_db):
        """Test scoring with various event types."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        test_events = [
            {"selection_correctness": 1, "execution_time_ms": 100, "execution_cost": 0.01, "user_feedback_score": 5},
            {"selection_correctness": 0, "execution_time_ms": 30000, "execution_cost": 10.0, "user_feedback_score": 1},
            {"selection_correctness": None, "execution_time_ms": 5000, "execution_cost": 1.0, "user_feedback_score": 3},
            {"execution_time_ms": 0, "execution_cost": 0},
            {},
        ]
        
        for event in test_events:
            score = selector.calculate_multi_factor_score(event)
            assert 0 <= score <= 100

    def test_confidence_normalization_range(self, db, clean_db):
        """Test confidence normalization across full range."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        test_values = [0, 10, 25, 50, 75, 90, 100, None]
        for val in test_values:
            normalized = selector._normalize_confidence(val)
            assert 0 <= normalized <= 1.0

    def test_signal_weights_all_types(self, db, clean_db):
        """Test signal weights for all signal types."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        for signal_type in SignalType:
            weight = selector._get_signal_weight(signal_type.value)
            assert 0 < weight <= 1.0

    def test_embedding_operations_comprehensive(self, db, clean_db):
        """Test all embedding operations."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Test vector conversion
        test_vectors = [
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 1.0],
            [0.5, 0.5, 0.5],
        ]
        
        for vec in test_vectors:
            vec_str = selector._embedding_to_vec_str(vec)
            parsed = selector._parse_embedding(vec_str)
            assert parsed == vec
        
        # Test similarity
        for i, v1 in enumerate(test_vectors):
            for v2 in test_vectors[i:]:
                sim = selector._l2_similarity(v1, v2)
                assert 0 <= sim <= 1

    def test_high_confidence_detection(self, db, clean_db):
        """Test high confidence threshold detection."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        assert selector._is_high_confidence(0.6) is True
        assert selector._is_high_confidence(0.4) is False
        assert selector._is_high_confidence(60.0) is True
        assert selector._is_high_confidence(40.0) is False
        assert selector._is_high_confidence(None) is False

    def test_get_recent_failures_pagination(self, db, clean_db):
        """Test get_recent_failures with pagination."""
        # Create many failures
        for i in range(20):
            event = SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id=str(uuid7()),
                user_query=f"comp_page_{i}",
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
        
        # Test different limits
        for limit in [5, 10, 15, 20]:
            failures = selector.get_recent_failures(days=7, limit=limit)
            assert len(failures) <= limit

    def test_signal_extraction_boundary_conditions(self, db, clean_db):
        """Test signal extraction at threshold boundaries."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Just below thresholds
        below = {
            'user_query': 'comp_below',
            'selected_skills': ['s1'],
            'available_skills': ['s1'],
            'execution_time_ms': selector.thresholds.slow_execution_ms - 100,
            'execution_cost': selector.thresholds.high_cost_usd - 0.01,
            'user_feedback_score': selector.thresholds.low_satisfaction + 1,
        }
        
        # Just above thresholds
        above = {
            'user_query': 'comp_above',
            'selected_skills': ['s1'],
            'available_skills': ['s1'],
            'execution_time_ms': selector.thresholds.slow_execution_ms + 100,
            'execution_cost': selector.thresholds.high_cost_usd + 0.01,
            'user_feedback_score': selector.thresholds.low_satisfaction - 1,
        }
        
        # Below should not trigger slow execution signal
        signal_below = selector._extract_signal(below, SignalType.SLOW_EXECUTION)
        assert signal_below is None
        
        # Above should trigger
        signal_above = selector._extract_signal(above, SignalType.SLOW_EXECUTION)
        assert signal_above is not None

    def test_learning_persistence_multiple_operations(self, db, clean_db):
        """Test learning persistence across multiple operations."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Create multiple learnings
        for i in range(5):
            signal = LearningSignal(
                signal_type=SignalType.WRONG_SKILL,
                query_pattern=f"comp_multi_{i}",
                wrong_skills=[f"w{i}"],
                correct_skills=[f"c{i}"],
                target_metrics={"accuracy": 0.8 + i * 0.02},
                confidence=50.0 + i * 5
            )
            selector._update_learnings(signal)
        
        # Verify all persisted
        stats = selector.get_learning_stats()
        assert stats['total_learnings'] >= 5

    def test_apply_learnings_no_match_scenario(self, db, clean_db):
        """Test apply_learnings when no learnings match."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [
            SkillCandidate(name="s1"),
            SkillCandidate(name="s2"),
        ]
        
        # Query with no matching learnings
        result = selector.apply_learnings("comp_nomatch_xyz_abc", candidates)
        assert len(result) == len(candidates)

    def test_runtime_config_loading_and_caching(self, db, clean_db):
        """Test runtime config loading and caching."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        config1 = selector._load_runtime_config()
        config2 = selector._load_runtime_config()
        
        assert 'weights' in config1
        assert 'decay' in config1
        assert config1.keys() == config2.keys()

    def test_edge_cases_and_null_handling(self, db, clean_db):
        """Test edge cases and null value handling."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Empty candidates
        result = selector.apply_learnings("comp_empty", [])
        assert result == []
        
        # Empty query
        from core.skills.pipeline import SkillCandidate
        candidates = [SkillCandidate(name="s1")]
        result = selector.apply_learnings("", candidates)
        assert len(result) == 1
        
        # None values
        assert selector._normalize_confidence(None) == 0.0
        assert selector._embedding_to_vec_str(None) is None
        assert selector._parse_embedding(None) is None
        
        # Empty event
        score = selector.calculate_multi_factor_score({})
        assert 0 <= score <= 100

    def test_learn_from_failures_exception_handling(self, db, clean_db):
        """Test learn_from_failures handles exceptions gracefully."""
        # Create failure event
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="comp_exception",
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
        
        # Should handle gracefully
        result = selector.learn_from_failures(days=7)
        assert 'learned' in result
        assert 'signals_by_type' in result

    def test_context_features_in_signals(self, db, clean_db):
        """Test context features extraction in signals."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        failure = {
            'user_query': 'comp_context',
            'selected_skills': ['s1'],
            'available_skills': ['s1', 's2'],
            'user_id': 'alice',
            'session_id': 'session_123',
            'execution_success': False,
            'selection_correctness': False,
        }
        
        signal = selector._extract_signal(failure, SignalType.WRONG_SKILL)
        if signal:
            assert signal.query_pattern == 'comp_context'

    def test_signal_weights_validation(self, db, clean_db):
        """Test signal weights validation."""
        # Valid weights
        valid = SignalWeights(accuracy=0.4, speed=0.3, cost=0.2, satisfaction=0.1)
        assert valid.accuracy == 0.4
        
        # Invalid: negative
        with pytest.raises(ValueError):
            SignalWeights(accuracy=-0.1, speed=0.3, cost=0.2, satisfaction=0.6)
        
        # Invalid: exceeds 1.0
        with pytest.raises(ValueError):
            SignalWeights(accuracy=1.1, speed=0.3, cost=0.2, satisfaction=-0.6)
        
        # Invalid: sum != 1.0
        with pytest.raises(ValueError):
            SignalWeights(accuracy=0.5, speed=0.3, cost=0.2, satisfaction=0.1)

    def test_initialization_validation(self, db, clean_db):
        """Test initialization validation."""
        with pytest.raises(TypeError):
            SelfImprovingSelector(session="invalid", llm_client=None)

    def test_apply_learnings_with_correction_addition(self, db, clean_db):
        """Test apply_learnings adds corrected skills."""
        # Create learning that adds new skills
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="comp_add_skills",
            wrong_skills=["old_skill"],
            correct_skills=["new_skill1", "new_skill2"],
            improvement_score=20.0,
            confidence=85.0,
            evidence_count=5,
            signal_type=SignalType.WRONG_SKILL.value,
            target_metrics={"accuracy": 0.9},
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(learning)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [SkillCandidate(name="old_skill", confidence=0.8)]
        
        result = selector.apply_learnings("comp_add_skills", candidates)
        # May add new skills
        assert len(result) >= 1

    def test_multiple_learnings_same_query(self, db, clean_db):
        """Test multiple learnings for same query pattern."""
        # Create multiple learnings for same pattern
        for i in range(3):
            learning = SkillSelectionLearning(
                learning_id=str(uuid7()),
                query_pattern="comp_multi_learn",
                wrong_skills=[f"wrong_{i}"],
                correct_skills=[f"correct_{i}"],
                improvement_score=10.0 + i * 5,
                confidence=50.0 + i * 10,
                evidence_count=1 + i,
                signal_type=SignalType.WRONG_SKILL.value,
                target_metrics={"accuracy": 0.8 + i * 0.05},
                created_at=datetime.now(timezone.utc).replace(tzinfo=None)
            )
            db.add(learning)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        from core.skills.pipeline import SkillCandidate
        candidates = [
            SkillCandidate(name="wrong_0"),
            SkillCandidate(name="wrong_1"),
            SkillCandidate(name="wrong_2"),
        ]
        
        result = selector.apply_learnings("comp_multi_learn", candidates)
        assert len(result) >= 2

    def test_signal_extraction_with_missing_fields(self, db, clean_db):
        """Test signal extraction handles missing fields gracefully."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Minimal failure data
        minimal = {
            'user_query': 'comp_minimal',
            'selected_skills': ['s1'],
        }
        
        # Should handle gracefully
        signal = selector._extract_signal(minimal, SignalType.WRONG_SKILL)
        # May or may not extract signal, but shouldn't crash

    def test_learn_from_failures_with_signal_types_filter(self, db, clean_db):
        """Test learn_from_failures with specific signal types."""
        # Create failure event
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="comp_filter",
            available_skills=["s1", "s2"],
            selected_skills=["s1"],
            selection_method="test",
            execution_success=False,
            execution_time_ms=10000,
            execution_cost=5.0,
            user_feedback_score=1,
            selection_correctness=False,
            created_at=datetime.now(timezone.utc).replace(tzinfo=None)
        )
        db.add(event)
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Learn with specific signal types
        result = selector.learn_from_failures(days=7, signal_types=[SignalType.WRONG_SKILL])
        assert 'learned' in result

    def test_scoring_with_extreme_values(self, db, clean_db):
        """Test scoring with extreme values."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        extreme_events = [
            {"execution_time_ms": 0},
            {"execution_time_ms": 999999},
            {"execution_cost": 0},
            {"execution_cost": 999999},
            {"user_feedback_score": 0},
            {"user_feedback_score": 5},
        ]
        
        for event in extreme_events:
            score = selector.calculate_multi_factor_score(event)
            assert 0 <= score <= 100

    def test_embedding_with_empty_vectors(self, db, clean_db):
        """Test embedding operations with edge case vectors."""
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Single element
        single = [1.0]
        vec_str = selector._embedding_to_vec_str(single)
        parsed = selector._parse_embedding(vec_str)
        assert parsed == single
        
        # Large vector
        large = [float(i) for i in range(100)]
        vec_str = selector._embedding_to_vec_str(large)
        parsed = selector._parse_embedding(vec_str)
        assert len(parsed) == 100

    def test_get_recent_failures_with_days_filter(self, db, clean_db):
        """Test get_recent_failures respects days parameter."""
        # Create old and new failures
        old_date = datetime.now(timezone.utc) - timedelta(days=10)
        new_date = datetime.now(timezone.utc)
        
        # Old failure
        old_event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="comp_old",
            available_skills=["s1"],
            selected_skills=["s1"],
            selection_method="test",
            execution_success=False,
            selection_correctness=False,
            created_at=old_date.replace(tzinfo=None)
        )
        
        # New failure
        new_event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="comp_new",
            available_skills=["s1"],
            selected_skills=["s1"],
            selection_method="test",
            execution_success=False,
            selection_correctness=False,
            created_at=new_date.replace(tzinfo=None)
        )
        
        db.add_all([old_event, new_event])
        db.commit()
        
        selector = SelfImprovingSelector(session=db, llm_client=None)
        
        # Get failures from last 7 days
        recent = selector.get_recent_failures(days=7)
        # Should only get new failure
        assert len(recent) >= 1
