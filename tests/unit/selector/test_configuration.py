"""Configuration and statistics tests."""

import uuid
import pytest

from api.models import Config, SkillSelectionLearning, SelectorGateResult


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
        db.query(SelectorGateResult).delete()
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
        
        db.add(SelectorGateResult(
            gate_id=f"gate-1-{uuid.uuid4().hex[:8]}",
            selector_version="v1",
            test_queries=[],
            test_count=10,
            verdict="PASS",
            new_avg_score=0.85,
            old_avg_score=0.80,
            improvement_pct=6.25,
        ))
        db.add(SelectorGateResult(
            gate_id=f"gate-2-{uuid.uuid4().hex[:8]}",
            selector_version="v2",
            test_queries=[],
            test_count=10,
            verdict="FAIL",
            new_avg_score=0.75,
            old_avg_score=0.80,
            improvement_pct=-6.25,
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
        db.query(SelectorGateResult).delete()
        db.commit()
        
        db.add(SelectorGateResult(
            gate_id=f"gate-null-{uuid.uuid4().hex[:8]}",
            selector_version="v1",
            test_queries=[],
            test_count=10,
            verdict="PASS",
            new_avg_score=0.85,
            old_avg_score=0.80,
            improvement_pct=None,
        ))
        db.commit()
        
        stats = self_improving.get_learning_stats()
        
        assert stats["regression_gates"]["total_gates"] == 1
        assert stats["regression_gates"]["passed"] == 1
        assert stats["regression_gates"]["avg_improvement_pct"] == 0.0

    def test_get_learning_stats_all_gates_passing(self, self_improving, db):
        """Test stats with all gates passing."""
        db.query(SkillSelectionLearning).delete()
        db.query(SelectorGateResult).delete()
        db.commit()
        
        for i in range(3):
            db.add(SelectorGateResult(
                gate_id=f"gate-pass-{i}-{uuid.uuid4().hex[:8]}",
                selector_version=f"v{i}",
                test_queries=[],
                test_count=10,
                verdict="PASS",
                new_avg_score=0.85 + i * 0.01,
                old_avg_score=0.80,
                improvement_pct=5.0 + i,
            ))
        db.commit()
        
        stats = self_improving.get_learning_stats()
        
        assert stats["regression_gates"]["total_gates"] == 3
        assert stats["regression_gates"]["passed"] == 3
        assert stats["regression_gates"]["failed"] == 0
        assert stats["regression_gates"]["pass_rate"] == 1.0
        assert stats["regression_gates"]["avg_improvement_pct"] == 6.0
