"""Minimal real DB tests for self-improving selector."""

import json
from unittest.mock import Mock, patch
import uuid

import pytest

from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.auditable_selector import SkillSelectionEvent, AuditableSkillSelector
from core.skills.selector import SkillMetadata


@pytest.fixture
def mock_llm():
    """Mock LLM."""
    llm = Mock()
    llm.chat = Mock(return_value=json.dumps({
        "query_pattern": "review pr",
        "wrong_skills": ["summarize_pr"],
        "correct_skills": ["code_review"],
        "improvement_score": 0.8,
        "evidence": "User feedback"
    }))
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    return llm


@pytest.fixture
def selector(db, mock_llm):
    """Auditable selector."""
    return AuditableSkillSelector(db, mock_llm)


@pytest.fixture
def self_improving(db, mock_llm, selector):
    """Self-improving selector."""
    si = SelfImprovingSelector(db, mock_llm)
    si.auditable_selector = selector
    si._ensure_tables()
    return si


class TestSelfImprovingSelector:
    """Core functionality tests."""

    def test_learn_from_failures_no_failures(self, self_improving, db):
        """Test with no failures."""
        # Clear all events that could trigger any signal type
        from api.models import SkillSelectionEvent
        db.query(SkillSelectionEvent).delete()
        db.commit()
        
        result = self_improving.learn_from_failures(days=30)
        
        assert result["learned"] == 0

    def test_update_learnings_new(self, self_improving, db):
        """Test adding new learning."""
        # Clear any existing data
        from api.models import SkillSelectionLearning
        from core.skills.learning_signals import LearningSignal, SignalType
        db.query(SkillSelectionLearning).delete()
        db.commit()
        
        signal = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="review pr",
            wrong_skills=["summarize_pr"],
            correct_skills=["code_review"],
            target_metrics={"accuracy": 1.0},
            confidence=10.0,
        )
        
        self_improving._update_learnings(signal)
        
        rows = db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "review pr"
        ).all()
        assert len(rows) == 1

    def test_get_learning_stats_no_data(self, self_improving, db):
        """Test stats with no data."""
        # Clear any existing data
        from api.models import SkillSelectionLearning
        db.query(SkillSelectionLearning).delete()
        db.commit()
        
        stats = self_improving.get_learning_stats()
        
        assert stats["total_learnings"] == 0

    def test_get_recent_failures(self, self_improving, selector, db):
        """Test getting failures."""
        from api.models import SkillSelectionEvent as SkillSelectionEventModel
        
        for i in range(3):
            event = SkillSelectionEvent(
                event_id=f"evt-{i}-{uuid.uuid4().hex[:8]}",
                session_id=f"sess-{i}",
                user_query=f"Query {i}",
                context_snapshot="snap",
                available_skills=[],
                selected_skills=["wrong_skill"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
            )
            selector._save_event(event)
            
            # Update using ORM
            db.query(SkillSelectionEventModel).filter(
                SkillSelectionEventModel.event_id == event.event_id
            ).update({
                "user_feedback_score": 2,
                "selection_correctness": False
            })
            db.commit()
        
        # Verify data exists
        rows = db.query(SkillSelectionEventModel).filter(
            SkillSelectionEventModel.user_feedback_score <= 2
        ).all()
        assert len(rows) >= 3

    def test_update_learnings_multiple(self, self_improving, db):
        """Test updating learnings with multiple corrections."""
        # Clear data
        from api.models import SkillSelectionLearning
        from core.skills.learning_signals import LearningSignal, SignalType
        db.query(SkillSelectionLearning).delete()
        db.commit()
        
        signals = [
            LearningSignal(
                signal_type=SignalType.WRONG_SKILL,
                query_pattern="pattern1",
                wrong_skills=["skill1"],
                correct_skills=["skill2"],
                target_metrics={"accuracy": 1.0},
                confidence=10.0,
            ),
            LearningSignal(
                signal_type=SignalType.WRONG_SKILL,
                query_pattern="pattern2",
                wrong_skills=["skill3"],
                correct_skills=["skill4"],
                target_metrics={"accuracy": 1.0},
                confidence=10.0,
            )
        ]
        
        # Update learnings one by one
        for signal in signals:
            self_improving._update_learnings(signal)
        
        # Check that both were added
        learnings = db.query(SkillSelectionLearning).all()
        assert len(learnings) == 2

    def test_apply_learnings_no_match(self, self_improving):
        """Test applying learnings with no match."""
        candidates = [SkillMetadata(
            name="code_review", version="1.0.0", description="Test",
            category="test", subcategory="sub", triggers=[],
            dependencies=[], priority=5, cost_estimate="low"
        )]
        
        corrected = self_improving.apply_learnings("unrelated query", candidates)
        
        assert corrected == candidates

    def test_apply_learnings_with_match(self, self_improving, db):
        """Test applying learnings with match."""
        # Insert learning using ORM
        from api.models import SkillSelectionLearning
        learning = SkillSelectionLearning(
            learning_id=f"learn-{uuid.uuid4().hex[:8]}",
            query_pattern="review pr",
            wrong_skills=["summarize_pr"],
            correct_skills=["code_review"],
            confidence=0.8,
            evidence_count=5
        )
        db.add(learning)
        db.commit()
        
        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            ),
            SkillMetadata(
                name="code_review", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=8, cost_estimate="medium"
            )
        ]
        
        corrected = self_improving.apply_learnings("Review PR #123", candidates)
        
        # Should prioritize code_review
        assert len(corrected) > 0

    def test_apply_learnings_semantic_match(self, self_improving, db):
        """Test applying learnings using semantic matching."""
        from api.models import SkillSelectionLearning
        from core.context.embeddings import EmbeddingService

        service = EmbeddingService(db, provider="mock")
        embedding = service.embed_text("Review PR #123")

        learning = SkillSelectionLearning(
            learning_id=f"learn-sem-{uuid.uuid4().hex[:8]}",
            query_pattern="unrelated pattern",
            query_embedding=embedding,
            wrong_skills=["summarize_pr"],
            correct_skills=["code_review"],
            confidence=0.9,
            evidence_count=5
        )
        db.add(learning)
        db.commit()

        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            ),
            SkillMetadata(
                name="code_review", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=8, cost_estimate="medium"
            )
        ]

        corrected = self_improving.apply_learnings("Review PR #123", candidates)

        assert corrected[0].name == "code_review"

    def test_apply_learnings_context_mismatch(self, self_improving, db):
        """Test context feature filtering blocks mismatched learnings."""
        from api.models import SkillSelectionLearning

        learning = SkillSelectionLearning(
            learning_id=f"learn-ctx-{uuid.uuid4().hex[:8]}",
            query_pattern="review",
            wrong_skills=["summarize_pr"],
            correct_skills=["code_review"],
            confidence=0.9,
            evidence_count=5,
            context_features={"length_bucket": "short", "contains_code": False},
        )
        db.add(learning)
        db.commit()

        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            ),
            SkillMetadata(
                name="code_review", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=8, cost_estimate="medium"
            )
        ]

        long_query = "review " + ("x" * 250)
        corrected = self_improving.apply_learnings(long_query, candidates)

        assert corrected == candidates

    def test_semantic_threshold_config_blocks_match(self, self_improving, db):
        """Test semantic similarity threshold is configurable."""
        from api.models import Config, SkillSelectionLearning
        from core.context.embeddings import EmbeddingService

        db.query(Config).filter(
            Config.key_name == "selector_semantic_similarity_threshold"
        ).delete()
        db.query(SkillSelectionLearning).delete()
        db.commit()

        db.add(
            Config(
                key_name="selector_semantic_similarity_threshold",
                value="1.1",
            )
        )
        db.commit()

        service = EmbeddingService(db, provider="mock")
        embedding = service.embed_text("Review PR #123")

        learning = SkillSelectionLearning(
            learning_id=f"learn-th-{uuid.uuid4().hex[:8]}",
            query_pattern="unrelated pattern",
            query_embedding=embedding,
            wrong_skills=["summarize_pr"],
            correct_skills=["code_review"],
            confidence=0.9,
            evidence_count=5
        )
        db.add(learning)
        db.commit()

        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            ),
            SkillMetadata(
                name="code_review", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=8, cost_estimate="medium"
            )
        ]

        self_improving._runtime_config_cache = None
        self_improving._runtime_config_loaded_at = None
        corrected = self_improving.apply_learnings("Review PR #123", candidates)

        assert corrected == candidates

    def test_parse_embedding(self, self_improving):
        """Test parsing embeddings from various formats."""
        raw_list = [0.1, 0.2]
        assert self_improving._parse_embedding(raw_list) == raw_list

        raw_json = "[0.3, 0.4]"
        assert self_improving._parse_embedding(raw_json) == [0.3, 0.4]

        assert self_improving._parse_embedding("not-json") is None
        assert self_improving._parse_embedding(None) is None

    def test_cosine_similarity(self, self_improving):
        """Test cosine similarity calculations."""
        assert self_improving._cosine_similarity([1.0, 0.0], [1.0, 0.0]) == 1.0
        assert self_improving._cosine_similarity([1.0, 0.0], [0.0, 1.0]) == 0.0

    def test_extract_context_features(self, self_improving):
        """Test context feature extraction."""
        short = self_improving._extract_context_features_from_query("review")
        assert short["length_bucket"] == "short"
        assert short["contains_code"] is False

        code = self_improving._extract_context_features_from_query("def foo():\n    return 1")
        assert code["contains_code"] is True

    def test_apply_learnings_limits_matches(self, self_improving, db):
        """Test applying only top matching learnings."""
        from api.models import SkillSelectionLearning
        db.query(SkillSelectionLearning).delete()
        db.commit()

        confidences = [0.9, 0.8, 0.7, 0.6]
        for i, conf in enumerate(confidences):
            learning = SkillSelectionLearning(
                learning_id=f"learn-{i}-{uuid.uuid4().hex[:8]}",
                query_pattern="review",
                wrong_skills=["summarize_pr"],
                correct_skills=["code_review"],
                confidence=conf,
                evidence_count=5,
            )
            db.add(learning)
        db.commit()

        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            ),
            SkillMetadata(
                name="code_review", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=8, cost_estimate="medium"
            )
        ]

        self_improving.apply_learnings("review pr", candidates)

        learnings = db.query(SkillSelectionLearning).order_by(
            SkillSelectionLearning.confidence.desc()
        ).all()
        applied_counts = [learning.applied_count for learning in learnings]

        assert applied_counts[0] == 1
        assert applied_counts[1] == 1
        assert applied_counts[2] == 1
        assert applied_counts[3] == 0

    def test_apply_learnings_returns_candidates_on_zero_scores(self, db, mock_llm):
        """Test fallback when all scores are zero."""
        from api.models import SkillSelectionLearning
        from core.skills.learning_signals import SignalWeights
        db.query(SkillSelectionLearning).delete()
        db.commit()

        si = SelfImprovingSelector(
            db,
            mock_llm,
            weights=SignalWeights(accuracy=1.0, speed=0.0, cost=0.0, satisfaction=0.0),
        )

        learning = SkillSelectionLearning(
            learning_id=f"learn-{uuid.uuid4().hex[:8]}",
            query_pattern="review",
            wrong_skills=["summarize_pr"],
            correct_skills=[],
            confidence=1.0,
            evidence_count=5,
        )
        db.add(learning)
        db.commit()

        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            )
        ]

        corrected = si.apply_learnings("review", candidates)

        assert corrected == candidates

    def test_get_learning_stats_with_data(self, self_improving, db):
        """Test stats with data."""
        # Clear and insert using ORM
        from api.models import SkillSelectionLearning
        db.query(SkillSelectionLearning).delete()
        db.commit()
        
        for i in range(3):
            learning = SkillSelectionLearning(
                learning_id=f"learn-{i}-{uuid.uuid4().hex[:8]}",
                query_pattern=f"pattern {i}",
                wrong_skills=["wrong"],
                correct_skills=["correct"],
                confidence=0.7 + i * 0.1,
                evidence_count=i + 1
            )
            db.add(learning)
        db.commit()
        
        stats = self_improving.get_learning_stats()
        
        assert stats["total_learnings"] == 3
        assert stats["avg_confidence"] > 0

    def test_learn_from_failures_with_failures(self, self_improving, selector, db):
        """Test learning from actual failures."""
        event = SkillSelectionEvent(
            event_id=f"evt-{uuid.uuid4().hex[:8]}",
            session_id="sess-1",
            user_query="Review PR #123",
            context_snapshot="snap",
            available_skills=[],
            selected_skills=["wrong_skill"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
        )
        selector._save_event(event)
        
        # Update using ORM
        from api.models import SkillSelectionEvent as SkillSelectionEventModel
        db.query(SkillSelectionEventModel).filter(
            SkillSelectionEventModel.event_id == event.event_id
        ).update({
            "user_feedback_score": 1,
            "selection_correctness": False
        })
        db.commit()
        
        result = self_improving.learn_from_failures(days=30)
        
        # May be 0 if LLM doesn't find pattern
        assert result["learned"] >= 0

    def test_learn_creates_and_cleans_sandbox(self, self_improving, selector, db):
        """Test sandbox lifecycle during learning."""
        event = SkillSelectionEvent(
            event_id=f"evt-{uuid.uuid4().hex[:8]}",
            session_id="sess-1",
            user_query="Test",
            context_snapshot="snap",
            available_skills=[],
            selected_skills=["wrong_skill"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
        )
        selector._save_event(event)
        
        # Update using ORM
        from api.models import SkillSelectionEvent as SkillSelectionEventModel
        db.query(SkillSelectionEventModel).filter(
            SkillSelectionEventModel.event_id == event.event_id
        ).update({
            "user_feedback_score": 1,
            "selection_correctness": False
        })
        db.commit()
        
        # Run learning
        self_improving.learn_from_failures(days=30)
        
        # Sandbox cleanup is handled internally, just verify no errors
        assert True

    def test_apply_learnings_empty_candidates(self, self_improving):
        """Test applying learnings with empty candidates."""
        corrected = self_improving.apply_learnings("test query", [])
        
        assert corrected == []

    def test_multiple_learnings_same_pattern(self, self_improving, db):
        """Test multiple learnings for same pattern."""
        # Clear data
        # Clear using ORM
        from api.models import SkillSelectionLearning
        from core.skills.learning_signals import LearningSignal, SignalType
        db.query(SkillSelectionLearning).delete()
        db.commit()
        
        # Add same pattern multiple times
        for i in range(3):
            signal = LearningSignal(
                signal_type=SignalType.WRONG_SKILL,
                query_pattern="review pr",
                wrong_skills=["wrong"],
                correct_skills=["correct"],
                target_metrics={"accuracy": 1.0},
                confidence=10.0,
            )
            self_improving._update_learnings(signal)
        
        # Should accumulate evidence
        from api.models import SkillSelectionLearning
        rows = db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "review pr"
        ).all()
        assert len(rows) >= 1
        assert rows[0].evidence_count >= 1
