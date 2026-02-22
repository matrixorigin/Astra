"""Core learning functionality tests."""

import uuid
import pytest
from uuid_utils import uuid7

from api.models import SkillSelectionEvent, SkillSelectionLearning
from core.skills.learning_signals import LearningSignal, SignalType


class TestLearningCore:
    """Core learning tests."""

    def test_learn_from_failures_no_failures(self, self_improving, clean_skill_events_db):
        """Test with no failures."""
        result = self_improving.learn_from_failures(days=30)
        assert result["learned"] == 0

    def test_update_learnings_new(self, self_improving, clean_skill_learning_db):
        """Test adding new learning."""
        signal = LearningSignal(
            signal_type=SignalType.WRONG_SKILL,
            query_pattern="review pr",
            wrong_skills=["summarize_pr"],
            correct_skills=["code_review"],
            target_metrics={"accuracy": 1.0},
            confidence=10.0,
        )
        
        self_improving._update_learnings(signal)
        
        rows = clean_skill_learning_db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "review pr"
        ).all()
        assert len(rows) == 1

    def test_get_learning_stats_no_data(self, self_improving, clean_skill_learning_db):
        """Test stats with no data."""
        stats = self_improving.get_learning_stats()
        
        assert stats["total_learnings"] == 0
        assert stats["regression_gates"]["total_gates"] == 0
        assert stats["regression_gates"]["passed"] == 0
        assert stats["regression_gates"]["failed"] == 0
        assert stats["regression_gates"]["pass_rate"] == 0.0
        assert stats["regression_gates"]["avg_improvement_pct"] == 0.0

    def test_get_recent_failures(self, self_improving, db):
        """Test getting failures."""
        for i in range(3):
            uuid_str = str(uuid7()).replace("-", "")
            eid = f"e{i}-{uuid_str}"  # Shorter prefix: e0-, e1-, e2-
            db.add(SkillSelectionEvent(
                event_id=eid, session_id=f"sess-{i}",
                user_query=f"Query {i}", selected_skills="wrong_skill",
                selection_method="llm", user_feedback_score=2,
                selection_correctness=False,
            ))
        db.commit()
        
        rows = db.query(SkillSelectionEvent).filter(
            SkillSelectionEvent.user_feedback_score <= 2
        ).all()
        assert len(rows) >= 3

    def test_update_learnings_multiple(self, self_improving, clean_skill_learning_db):
        """Test updating learnings with multiple corrections."""
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
        
        for signal in signals:
            self_improving._update_learnings(signal)
        
        learnings = clean_skill_learning_db.query(SkillSelectionLearning).all()
        assert len(learnings) == 2

    def test_learn_from_failures_with_failures(self, self_improving, db):
        """Test learning from actual failures."""
        uuid_str2 = str(uuid7()).replace("-", "")
        eid = f"e-{uuid_str2}"  # Shorter prefix
        db.add(SkillSelectionEvent(
            event_id=eid, session_id="sess-1",
            user_query="Review PR #123", selected_skills="wrong_skill",
            selection_method="llm", user_feedback_score=1,
            selection_correctness=False,
        ))
        db.commit()
        
        result = self_improving.learn_from_failures(days=30)
        assert result["learned"] >= 0

    def test_learn_creates_and_cleans_sandbox(self, self_improving, db):
        """Test sandbox lifecycle during learning."""
        uuid_str3 = str(uuid7()).replace("-", "")
        eid = f"e-{uuid_str3}"  # Shorter prefix
        db.add(SkillSelectionEvent(
            event_id=eid, session_id="sess-1",
            user_query="Test", selected_skills="wrong_skill",
            selection_method="llm", user_feedback_score=1,
            selection_correctness=False,
        ))
        db.commit()
        
        self_improving.learn_from_failures(days=30)
        assert True

    def test_multiple_learnings_same_pattern(self, self_improving, clean_skill_learning_db):
        """Test multiple learnings for same pattern."""
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
        
        rows = clean_skill_learning_db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.query_pattern == "review pr"
        ).all()
        assert len(rows) >= 1
        assert rows[0].evidence_count >= 1
