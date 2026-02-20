"""Tests for cross-model consistency verification."""

from unittest.mock import Mock

import pytest

from core.agents.consistency import (
    ConsistencyCheck,
    ConsistencyVerifier,
    SkillConsistencyPolicy,
    ToleranceClass,
)


def _mock_db():
    return Mock()


class TestToleranceClass:
    def test_tolerance_values(self):
        assert ToleranceClass.STRICT.value == "strict"
        assert ToleranceClass.SEMANTIC.value == "semantic"
        assert ToleranceClass.RELAXED.value == "relaxed"


class TestSkillConsistencyPolicy:
    def test_policy_creation(self):
        policy = SkillConsistencyPolicy(
            skill_id="code_review",
            tolerance=ToleranceClass.SEMANTIC,
            verification_model="gpt-3.5",
            max_retries=2,
        )
        assert policy.skill_id == "code_review"
        assert policy.tolerance == ToleranceClass.SEMANTIC
        assert policy.max_retries == 2


class TestConsistencyCheck:
    def test_check_passed(self):
        check = ConsistencyCheck(passed=True)
        assert check.passed is True
        assert check.reason is None
        assert check.score == 1.0

    def test_check_failed(self):
        check = ConsistencyCheck(
            passed=False,
            reason="Schema mismatch",
            score=0.3,
        )
        assert check.passed is False
        assert check.reason == "Schema mismatch"
        assert check.score == 0.3


class TestConsistencyVerifier:
    def test_check_structural_valid(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)

        output = {"action": "deploy", "target": "prod"}
        schema = {
            "type": "object",
            "required": ["action", "target"],
            "properties": {
                "action": {"type": "string"},
                "target": {"type": "string"},
            },
        }

        check = verifier.check_structural(output, schema)
        assert check.passed is True

    def test_check_structural_missing_field(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)

        output = {"action": "deploy"}  # Missing "target"
        schema = {
            "type": "object",
            "required": ["action", "target"],
            "properties": {
                "action": {"type": "string"},
                "target": {"type": "string"},
            },
        }

        check = verifier.check_structural(output, schema)
        assert check.passed is False
        assert "target" in check.reason

    def test_check_structural_wrong_type(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)

        output = {"action": "deploy", "target": 123}  # target should be string
        schema = {
            "type": "object",
            "required": ["action", "target"],
            "properties": {
                "action": {"type": "string"},
                "target": {"type": "string"},
            },
        }

        check = verifier.check_structural(output, schema)
        assert check.passed is False

    def test_check_structural_not_dict(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)

        output = "not a dict"
        schema = {"type": "object"}

        check = verifier.check_structural(output, schema)
        assert check.passed is False

    def test_check_semantic_no_llm(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db, llm_client=None)

        check = verifier.check_semantic("output text")
        assert check.passed is True

    def test_check_semantic_contradiction_with_llm(self):
        db = _mock_db()
        llm = Mock()
        # LLM says "yes" these contradict
        llm.chat.return_value = Mock(content="yes")
        verifier = ConsistencyVerifier(db, llm_client=llm)

        output = "The function should not be modified"
        prior = ["The function should be refactored"]

        check = verifier.check_semantic(output, prior_outputs=prior)
        assert check.passed is False
        assert "Contradicts" in check.reason

    def test_check_semantic_with_reference(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db, llm_client=Mock())

        output = "Deploy to production environment"
        reference = "Deploy to production"

        check = verifier.check_semantic(output, reference=reference)
        # Should have high similarity
        assert check.passed is True
        assert check.score > 0.5

    def test_record_compatibility(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)

        verifier.record_compatibility(
            task_type="code_review",
            model_a="gpt-4",
            model_b="gpt-3.5",
            compatible=True,
            score=0.92,
        )

        db.execute.assert_called_once()
        db.commit.assert_called_once()

    def test_get_compatibility_score_found(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=(0.92,)))

        verifier = ConsistencyVerifier(db)
        score = verifier.get_compatibility_score("code_review", "gpt-4", "gpt-3.5")

        assert score == 0.92

    def test_get_compatibility_score_not_found(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=None))

        verifier = ConsistencyVerifier(db)
        score = verifier.get_compatibility_score("code_review", "gpt-4", "gpt-3.5")

        assert score == 0.5  # Default unknown

    def test_should_failover_high_compatibility(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=(0.92,)))

        verifier = ConsistencyVerifier(db)
        result = verifier.should_failover("code_review", "gpt-4", "gpt-3.5")

        assert result is True

    def test_should_failover_low_compatibility(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=(0.65,)))

        verifier = ConsistencyVerifier(db)
        result = verifier.should_failover("code_review", "gpt-4", "gpt-3.5")

        assert result is False

    def test_should_failover_unknown(self):
        db = _mock_db()
        db.execute.return_value = Mock(fetchone=Mock(return_value=None))

        verifier = ConsistencyVerifier(db)
        result = verifier.should_failover("code_review", "gpt-4", "gpt-3.5")

        assert result is False  # 0.5 < 0.7

    def test_type_matches(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)

        assert verifier._type_matches("hello", "string") is True
        assert verifier._type_matches(42, "integer") is True
        assert verifier._type_matches(3.14, "number") is True
        assert verifier._type_matches(True, "boolean") is True
        assert verifier._type_matches([], "array") is True
        assert verifier._type_matches({}, "object") is True

        assert verifier._type_matches(42, "string") is False
        assert verifier._type_matches("hello", "integer") is False

    def test_contradicts_with_llm(self):
        db = _mock_db()
        llm = Mock()
        llm.chat.return_value = Mock(content="yes")
        verifier = ConsistencyVerifier(db, llm_client=llm)

        assert verifier._contradicts("should not modify", "should modify") is True
        llm.chat.assert_called()

    def test_contradicts_no_llm_uses_similarity(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)  # No LLM

        # Identical text → high similarity → no contradiction
        assert verifier._contradicts("same message", "same message") is False

    def test_contradicts_no_llm_low_similarity(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)  # No LLM

        # Completely different → low similarity → contradiction
        assert verifier._contradicts("alpha beta gamma", "x y z") is True

    def test_semantic_similarity(self):
        db = _mock_db()
        verifier = ConsistencyVerifier(db)

        # Identical
        sim = verifier._semantic_similarity("hello world", "hello world")
        assert sim == 1.0

        # Partial overlap
        sim = verifier._semantic_similarity("hello world", "hello there")
        assert 0 < sim < 1

        # No overlap
        sim = verifier._semantic_similarity("hello", "goodbye")
        assert sim < 0.5
