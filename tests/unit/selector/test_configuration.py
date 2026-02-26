"""Configuration and statistics tests."""

import pytest
from uuid_utils import uuid7

from api.models import Config, SkillSelectionLearning, GateResult


class TestConfiguration:
    """Test configuration management."""

    def test_get_learning_stats_no_data(self, self_improving, clean_skill_learning_db):
        """Test stats with no data."""
        stats = self_improving.get_learning_stats()

        assert stats["total_learnings"] == 0
        assert stats["regression_gates"]["total_gates"] == 0
        assert stats["regression_gates"]["passed"] == 0
        assert stats["regression_gates"]["failed"] == 0
        assert stats["regression_gates"]["pass_rate"] == 0.0
        assert stats["regression_gates"]["avg_improvement_pct"] == 0.0

    def test_get_learning_stats_with_data(self, self_improving, clean_skill_learning_db):
        """Test stats with learnings and mixed gate results."""
        db = clean_skill_learning_db
        db.query(Config).filter(
            Config.key_name == "selector_semantic_similarity_threshold"
        ).delete()
        db.commit()

        for i in range(3):
            db.add(SkillSelectionLearning(
                learning_id=f"l{i}-{str(uuid7()).replace('-', '')}",
                query_pattern=f"pattern {i}",
                wrong_skills=["wrong"],
                correct_skills=["correct"],
                confidence=0.7 + i * 0.1,
                evidence_count=i + 1,
            ))

        db.add(GateResult(
            gate_id=f"g1-{str(uuid7()).replace('-', '')}",
            change_type="selector", change_id="v1",
            sessions_tested=10, score_delta=8.0, passed=1,
        ))
        db.add(GateResult(
            gate_id=f"g2-{str(uuid7()).replace('-', '')}",
            change_type="selector", change_id="v2",
            sessions_tested=10, score_delta=-2.0, passed=0,
        ))
        db.commit()

        stats = self_improving.get_learning_stats()

        assert stats["total_learnings"] == 3
        assert stats["avg_confidence"] > 0
        assert stats["semantic_similarity_threshold"] == 0.78
        assert stats["regression_gates"]["total_gates"] == 2
        assert stats["regression_gates"]["passed"] == 1
        assert stats["regression_gates"]["failed"] == 1
        assert stats["regression_gates"]["pass_rate"] == 0.5
        assert stats["regression_gates"]["avg_improvement_pct"] == 3.0  # (8.0 + -2.0) / 2

    def test_get_learning_stats_with_null_score_delta(self, self_improving, clean_skill_learning_db):
        """Test that NULL score_delta is excluded from avg calculation."""
        db = clean_skill_learning_db
        db.add(GateResult(
            gate_id=f"gn-{str(uuid7()).replace('-', '')}",
            change_type="selector", change_id="v1",
            sessions_tested=10, score_delta=None, passed=1,
        ))
        db.commit()

        stats = self_improving.get_learning_stats()

        assert stats["regression_gates"]["total_gates"] == 1
        assert stats["regression_gates"]["passed"] == 1
        assert stats["regression_gates"]["avg_improvement_pct"] == 0.0

    def test_get_learning_stats_all_gates_passing(self, self_improving, clean_skill_learning_db):
        """Test stats when all gates pass."""
        db = clean_skill_learning_db
        for i in range(3):
            db.add(GateResult(
                gate_id=f"g{i}-{str(uuid7()).replace('-', '')}",
                change_type="selector", change_id=f"v{i}",
                sessions_tested=10, score_delta=5.0 + i, passed=1,
            ))
        db.commit()

        stats = self_improving.get_learning_stats()

        assert stats["regression_gates"]["total_gates"] == 3
        assert stats["regression_gates"]["passed"] == 3
        assert stats["regression_gates"]["failed"] == 0
        assert stats["regression_gates"]["pass_rate"] == 1.0
        assert stats["regression_gates"]["avg_improvement_pct"] == 6.0
