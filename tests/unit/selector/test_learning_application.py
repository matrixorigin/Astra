"""Learning application and matching tests."""

import uuid
import pytest
from uuid_utils import uuid7

from api.models import SkillSelectionLearning
from core.skills.selector import SkillMetadata
from core.utils.id_generator import generate_learning_id


class TestLearningApplication:
    """Test applying learnings to skill selection."""

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
        learning = SkillSelectionLearning(
            learning_id=generate_learning_id(),
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
        assert len(corrected) > 0

    def test_apply_learnings_empty_candidates(self, self_improving):
        """Test applying learnings with empty candidates."""
        corrected = self_improving.apply_learnings("test query", [])
        assert corrected == []

    def test_apply_learnings_limits_matches(self, self_improving, clean_skill_learning_db):
        """Test applying only top matching learnings."""
        confidences = [0.9, 0.8, 0.7, 0.6]
        for i, conf in enumerate(confidences):
            learning = SkillSelectionLearning(
                learning_id=generate_learning_id(),
                query_pattern="review",
                wrong_skills=["summarize_pr"],
                correct_skills=["code_review"],
                confidence=conf,
                evidence_count=5,
            )
            clean_skill_learning_db.add(learning)
        clean_skill_learning_db.commit()

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

        learnings = clean_skill_learning_db.query(SkillSelectionLearning).order_by(
            SkillSelectionLearning.confidence.desc()
        ).all()
        applied_counts = [learning.applied_count for learning in learnings]

        assert applied_counts[0] == 1
        assert applied_counts[1] == 1
        assert applied_counts[2] == 1
        assert applied_counts[3] == 0

    def test_apply_learnings_returns_candidates_on_zero_scores(self, clean_skill_learning_db, mock_llm_selector):
        """Test fallback when all scores are zero."""
        from core.skills.learning_signals import SignalWeights

        si = self_improving = type('obj', (object,), {
            'db': clean_skill_learning_db,
            'llm': mock_llm_selector,
        })()
        from core.skills.self_improving_selector import SelfImprovingSelector
        si = SelfImprovingSelector(
            lambda: clean_skill_learning_db,
            mock_llm_selector,
            weights=SignalWeights(accuracy=1.0, speed=0.0, cost=0.0, satisfaction=0.0),
        )

        learning = SkillSelectionLearning(
            learning_id=generate_learning_id(),
            query_pattern="review",
            wrong_skills=["summarize_pr"],
            correct_skills=[],
            confidence=1.0,
            evidence_count=5,
        )
        clean_skill_learning_db.add(learning)
        clean_skill_learning_db.commit()

        candidates = [
            SkillMetadata(
                name="summarize_pr", version="1.0.0", description="Test",
                category="test", subcategory="sub", triggers=[],
                dependencies=[], priority=6, cost_estimate="low"
            )
        ]

        corrected = si.apply_learnings("review", candidates)
        assert corrected == candidates

    def test_apply_learnings_does_not_mutate_input(self, self_improving, db):
        """apply_learnings should not modify the input candidates list."""
        from core.skills.pipeline import SkillCandidate
        from datetime import datetime, timezone

        learning = SkillSelectionLearning(
            learning_id="mut_test",
            query_pattern="mutate test",
            wrong_skills=["skill_a"],
            correct_skills=["skill_b"],
            confidence=0.95,
            evidence_count=5,
            applied_count=0,
            signal_type="wrong_skill",
            is_active=1,
            created_at=datetime.now(timezone.utc),
        )
        db.add(learning)
        db.flush()

        original = [
            SkillCandidate(name="skill_a"),
            SkillCandidate(name="skill_c"),
        ]
        original_copy = list(original)
        self_improving.apply_learnings("mutate test", original)
        assert len(original) == len(original_copy)
        for a, b in zip(original, original_copy):
            assert a.name == b.name

    def test_apply_learnings_deterministic_order_on_tie(self, self_improving, db):
        """Same-score candidates must be sorted by name for deterministic output."""
        learning = SkillSelectionLearning(
            learning_id=generate_learning_id(),
            query_pattern="deploy",
            wrong_skills=[],
            correct_skills=["zz_deploy", "aa_deploy"],
            signal_type="wrong_skill",
            confidence=0.9,
            evidence_count=5,
            applied_count=0,
            is_active=1,
        )
        db.add(learning)
        db.commit()

        from core.skills.pipeline import SkillCandidate
        candidates = [
            SkillCandidate(name="zz_deploy"),
            SkillCandidate(name="aa_deploy"),
        ]
        result = self_improving.apply_learnings("deploy service", candidates)
        names = [c.name for c in result]

        result2 = self_improving.apply_learnings("deploy service", candidates)
        names2 = [c.name for c in result2]
        assert names == names2
        assert names.index("aa_deploy") < names.index("zz_deploy")
