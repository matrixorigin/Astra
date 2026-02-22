"""Safety and robustness tests."""

import uuid
import pytest
from unittest.mock import Mock

from api.models import SkillSelectionLearning


class TestSafety:
    """Test safety features."""

    def test_rollback_learnings_by_ids(self, self_improving, db):
        """Rollback specific learnings by ID."""
        from uuid_utils import uuid7

        lid = str(uuid7())
        db.add(SkillSelectionLearning(
            learning_id=lid, query_pattern="test", wrong_skills=["a"],
            correct_skills=["b"], confidence=50, signal_type="wrong_skill",
        ))
        db.commit()

        count = self_improving.rollback_learnings(learning_ids=[lid])
        assert count == 1

        row = db.get(SkillSelectionLearning, lid)
        assert row.is_active == 0

    def test_rollback_learnings_no_args(self, self_improving):
        """Rollback with no args should be a no-op."""
        assert self_improving.rollback_learnings() == 0

    def test_apply_learnings_skips_inactive(self, self_improving, db):
        """Inactive learnings should not affect scoring."""
        from core.skills.selector import SkillMetadata
        from uuid_utils import uuid7

        db.add(SkillSelectionLearning(
            learning_id=str(uuid7()), query_pattern="deploy k8s",
            wrong_skills=["summarize_pr"], correct_skills=["deploy"],
            confidence=90, signal_type="wrong_skill", is_active=0,
        ))
        db.commit()

        candidates = [SkillMetadata(
            name="summarize_pr", version="1.0.0", description="",
            category="test", subcategory="sub", triggers=[],
            dependencies=[], priority=5, cost_estimate="low",
        )]
        result = self_improving.apply_learnings("deploy k8s cluster", candidates)
        assert result[0].name == "summarize_pr"

    def test_ensure_tables_rollback_on_failure(self, clean_skill_learning_db, mock_llm_selector):
        """_ensure_tables should rollback on DDL failure, not leave session dirty."""
        from core.skills.self_improving_selector import SelfImprovingSelector
        
        si = SelfImprovingSelector(clean_skill_learning_db, mock_llm_selector)
        si._ensure_tables()

        original_commit = clean_skill_learning_db.commit
        clean_skill_learning_db.commit = Mock(side_effect=RuntimeError("DDL failed"))
        try:
            with pytest.raises(RuntimeError, match="DDL failed"):
                SelfImprovingSelector(clean_skill_learning_db, mock_llm_selector)._ensure_tables()
        finally:
            clean_skill_learning_db.commit = original_commit

        from sqlalchemy import text
        result = clean_skill_learning_db.execute(text("SELECT 1")).scalar()
        assert result == 1

    def test_no_dual_scale_confidence_method(self, self_improving):
        """Dead _is_high_confidence (dual-scale) must not exist."""
        assert not hasattr(self_improving, "_is_high_confidence")

    def test_learn_skips_high_multi_factor_score_events(self, self_improving, db):
        """Events scoring above threshold are skipped by learn_from_failures."""
        from api.models import SkillSelectionEvent
        from uuid_utils import uuid7
        from datetime import datetime, timezone

        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="high score event",
            available_skills=["s1"],
            selected_skills=["s1"],
            selection_method="test",
            execution_success=True,
            selection_correctness=1,
            execution_time_ms=500,
            execution_cost=0.01,
            user_feedback_score=2,
            created_at=datetime.now(timezone.utc).replace(tzinfo=None),
        )
        db.add(event)
        db.commit()

        result = self_improving.learn_from_failures(days=1)
        assert result["skipped_high_score"] >= 1

    def test_multi_factor_score_handles_none_fields(self, self_improving):
        """calculate_multi_factor_score must not crash on None metric values."""
        event = {
            "selection_correctness": 1,
            "execution_time_ms": None,
            "execution_cost": None,
            "user_feedback_score": None,
        }
        score = self_improving.calculate_multi_factor_score(event)
        assert score > 0
