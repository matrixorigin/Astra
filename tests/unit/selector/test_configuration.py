"""Configuration and statistics tests."""

import uuid
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

    def test_get_learning_stats_with_data(self, self_improving, db):
        """Test stats with data."""
        db.query(Config).filter(
            Config.key_name == "selector_semantic_similarity_threshold"
        ).delete()
        db.query(SkillSelectionLearning).delete()
        db.query(GateResult).filter(GateResult.change_type == "selector").delete()
        db.commit()
        
        for i in range(3):
            uuid_str = str(uuid7()).replace("-", "")
            learning = SkillSelectionLearning(
                learning_id=f"l{i}-{uuid_str}",
                query_pattern=f"pattern {i}",
                wrong_skills=["wrong"],
                correct_skills=["correct"],
                confidence=0.7 + i * 0.1,
                evidence_count=i + 1
            )
            db.add(learning)
        
        uuid_str = str(uuid7()).replace("-", "")
        db.add(GateResult(
            gate_id=f"g1-{uuid_str}",
            change_type="selector",
            change_id="v1",
            sessions_tested=10,
            score_delta=6.25,
            passed=1,
        ))
        uuid_str2 = str(uuid7()).replace("-", "")
        db.add(GateResult(
            gate_id=f"g2-{uuid_str2}",
            change_type="selector",
            change_id="v2",
            sessions_tested=10,
            score_delta=-6.25,
            passed=0,
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
        assert stats["regression_gates"]["avg_improvement_pct"] == 0.0

    def test_get_learning_stats_with_null_improvement(self, self_improving, db):
        """Test stats with NULL improvement_pct."""
        db.query(SkillSelectionLearning).delete()
        db.query(GateResult).filter(GateResult.change_type == "selector").delete()
        db.commit()
        
        uuid_str3 = str(uuid7()).replace("-", "")
        db.add(GateResult(
            gate_id=f"gn-{uuid_str3}",
            change_type="selector",
            change_id="v1",
            sessions_tested=10,
            score_delta=None,
            passed=1,
        ))
        db.commit()
        
        stats = self_improving.get_learning_stats()
        
        assert stats["regression_gates"]["total_gates"] == 1
        assert stats["regression_gates"]["passed"] == 1
        assert stats["regression_gates"]["avg_improvement_pct"] == 0.0

    def test_get_learning_stats_all_gates_passing(self, self_improving, db):
        """Test stats with all gates passing."""
        db.query(SkillSelectionLearning).delete()
        db.query(GateResult).filter(GateResult.change_type == "selector").delete()
        db.commit()
        
        for i in range(3):
            uuid_str = str(uuid7()).replace("-", "")
            db.add(GateResult(
                gate_id=f"g{i}-{uuid_str}",
                change_type="selector",
                change_id=f"v{i}",
                sessions_tested=10,
                score_delta=5.0 + i,
                passed=1,
            ))
        db.commit()
        
        stats = self_improving.get_learning_stats()
        
        assert stats["regression_gates"]["total_gates"] == 3
        assert stats["regression_gates"]["passed"] == 3
        assert stats["regression_gates"]["failed"] == 0
        assert stats["regression_gates"]["pass_rate"] == 1.0
        assert stats["regression_gates"]["avg_improvement_pct"] == 6.0
