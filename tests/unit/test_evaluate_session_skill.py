"""Tests for evaluate_session skill."""

from unittest.mock import MagicMock

import pytest

from skills.evaluate_session.skill import (
    EvaluateSessionInput,
    EvaluateSessionSkill,
)


class TestCalculateMetrics:
    """Test _calculate_metrics logic."""

    def test_empty_rows(self):
        skill = EvaluateSessionSkill()
        metrics = skill._calculate_metrics([], "test-session")

        assert metrics["session_id"] == "test-session"
        assert metrics["total_events"] == 0
        assert metrics["user_queries"] == 0
        assert metrics["llm_calls"] == 0
        assert metrics["tokens"]["total"] == 0
        assert metrics["tokens"]["avg_per_call"] == 0

    def test_token_aggregation(self):
        skill = EvaluateSessionSkill()
        rows = [
            {
                "event_type": "user_query",
                "token_usage": None,
                "llm_model_used": None,
                "skill_name": None,
                "content": "hello",
            },
            {
                "event_type": "llm_response",
                "token_usage": '{"prompt": 100, "completion": 50}',
                "llm_model_used": "gpt-4",
                "skill_name": None,
                "content": "hi",
            },
            {
                "event_type": "llm_response",
                "token_usage": '{"prompt": 200, "completion": 100}',
                "llm_model_used": "gpt-4",
                "skill_name": None,
                "content": "bye",
            },
        ]

        metrics = skill._calculate_metrics(rows, "test-session")

        assert metrics["user_queries"] == 1
        assert metrics["llm_calls"] == 2
        assert metrics["tokens"]["prompt"] == 300
        assert metrics["tokens"]["completion"] == 150
        assert metrics["tokens"]["total"] == 450
        assert metrics["tokens"]["avg_per_call"] == 225

    def test_skill_counting(self):
        skill = EvaluateSessionSkill()
        rows = [
            {
                "event_type": "tool_call",
                "token_usage": None,
                "llm_model_used": None,
                "skill_name": "search",
                "content": None,
            },
            {
                "event_type": "tool_call",
                "token_usage": None,
                "llm_model_used": None,
                "skill_name": "search",
                "content": None,
            },
            {
                "event_type": "tool_call",
                "token_usage": None,
                "llm_model_used": None,
                "skill_name": "read_file",
                "content": None,
            },
        ]

        metrics = skill._calculate_metrics(rows, "test-session")

        assert metrics["skills"]["unique"] == 2
        assert metrics["skills"]["total_calls"] == 3
        assert metrics["skills"]["breakdown"] == {"search": 2, "read_file": 1}

    def test_token_usage_as_dict(self):
        """token_usage can be dict (already parsed by DB driver)."""
        skill = EvaluateSessionSkill()
        rows = [
            {
                "event_type": "llm_response",
                "token_usage": {"prompt": 500, "completion": 200},
                "llm_model_used": "gpt-4",
                "skill_name": None,
                "content": "response",
            },
        ]

        metrics = skill._calculate_metrics(rows, "test-session")

        assert metrics["tokens"]["prompt"] == 500
        assert metrics["tokens"]["completion"] == 200

    def test_malformed_token_usage_ignored(self):
        """Malformed token_usage should be skipped, not crash."""
        skill = EvaluateSessionSkill()
        rows = [
            {
                "event_type": "llm_response",
                "token_usage": "not-valid-json",
                "llm_model_used": "gpt-4",
                "skill_name": None,
                "content": "response",
            },
        ]

        metrics = skill._calculate_metrics(rows, "test-session")

        assert metrics["llm_calls"] == 0  # Not counted due to parse failure
        assert metrics["tokens"]["total"] == 0


class TestGenerateAssessment:
    """Test assessment logic."""

    @pytest.mark.parametrize(
        "tokens_per_query,expected",
        [
            (5000, "excellent"),
            (9999, "excellent"),
            (10000, "good"),
            (19999, "good"),
            (20000, "moderate"),
            (39999, "moderate"),
            (40000, "needs_improvement"),
            (100000, "needs_improvement"),
        ],
    )
    def test_token_efficiency_thresholds(self, tokens_per_query, expected):
        skill = EvaluateSessionSkill()
        metrics = {
            "tokens": {"total": tokens_per_query},
            "user_queries": 1,
            "llm_calls": 2,
        }

        assessment = skill._generate_assessment(metrics)

        assert assessment["token_efficiency"] == expected

    @pytest.mark.parametrize(
        "calls_per_query,expected",
        [
            (1.0, "excellent"),
            (2.0, "excellent"),
            (2.1, "good"),
            (4.0, "good"),
            (4.1, "moderate"),
            (6.0, "moderate"),
            (6.1, "needs_improvement"),
            (10.0, "needs_improvement"),
        ],
    )
    def test_call_efficiency_thresholds(self, calls_per_query, expected):
        skill = EvaluateSessionSkill()
        metrics = {
            "tokens": {"total": 5000},  # excellent token efficiency
            "user_queries": 10,
            "llm_calls": int(calls_per_query * 10),
        }

        assessment = skill._generate_assessment(metrics)

        assert assessment["call_efficiency"] == expected

    def test_zero_queries_no_division_error(self):
        skill = EvaluateSessionSkill()
        metrics = {
            "tokens": {"total": 1000},
            "user_queries": 0,
            "llm_calls": 0,
        }

        assessment = skill._generate_assessment(metrics)

        assert assessment["tokens_per_query"] == 0
        assert assessment["calls_per_query"] == 0.0

    def test_overall_good_requires_both_good(self):
        skill = EvaluateSessionSkill()

        # Both excellent -> good
        metrics = {"tokens": {"total": 5000}, "user_queries": 1, "llm_calls": 2}
        assert skill._generate_assessment(metrics)["overall"] == "good"

        # Token excellent, call moderate -> needs_improvement
        metrics = {"tokens": {"total": 5000}, "user_queries": 1, "llm_calls": 5}
        assert skill._generate_assessment(metrics)["overall"] == "needs_improvement"

        # Token moderate, call excellent -> needs_improvement
        metrics = {"tokens": {"total": 30000}, "user_queries": 1, "llm_calls": 2}
        assert skill._generate_assessment(metrics)["overall"] == "needs_improvement"


class TestEventBreakdown:
    """Test _get_event_breakdown."""

    def test_breakdown_structure(self):
        skill = EvaluateSessionSkill()
        rows = [
            {
                "event_type": "user_query",
                "token_usage": None,
                "llm_model_used": None,
                "skill_name": None,
                "content": "hello",
            },
            {
                "event_type": "llm_response",
                "token_usage": '{"total": 500}',
                "llm_model_used": "gpt-4",
                "skill_name": None,
                "content": "hi",
            },
        ]

        breakdown = skill._get_event_breakdown(rows)

        assert len(breakdown) == 2
        assert breakdown[0] == {
            "index": 1,
            "type": "user_query",
            "model": None,
            "skill": None,
        }
        assert breakdown[1] == {
            "index": 2,
            "type": "llm_response",
            "model": "gpt-4",
            "skill": None,
            "tokens": 500,
        }


class TestExecute:
    """Test execute method with mocked database."""

    @pytest.mark.asyncio
    async def test_session_not_found(self):
        mock_db = MagicMock()
        mock_result = MagicMock()
        mock_result.mappings.return_value.all.return_value = []
        mock_db.execute.return_value = mock_result

        skill = EvaluateSessionSkill(db=mock_db)
        result = await skill.execute(EvaluateSessionInput(target_session_id="nonexistent"))

        assert result.success is False
        assert "No events found" in result.error

    @pytest.mark.asyncio
    async def test_successful_evaluation(self):
        mock_db = MagicMock()
        mock_result = MagicMock()
        mock_result.mappings.return_value.all.return_value = [
            {
                "event_type": "user_query",
                "token_usage": None,
                "llm_model_used": None,
                "skill_name": None,
                "content": "hello",
            },
            {
                "event_type": "llm_response",
                "token_usage": '{"prompt": 1000, "completion": 500}',
                "llm_model_used": "gpt-4",
                "skill_name": None,
                "content": "hi",
            },
        ]
        mock_db.execute.return_value = mock_result

        skill = EvaluateSessionSkill(db=mock_db)
        result = await skill.execute(
            EvaluateSessionInput(target_session_id="test-session", include_details=False)
        )

        assert result.success is True
        assert result.session_id == "test-session"
        assert result.user_queries == 1
        assert result.llm_calls == 1
        assert result.tokens["total"] == 1500
        assert result.assessment["token_efficiency"] == "excellent"
        assert result.event_breakdown is None

    @pytest.mark.asyncio
    async def test_with_details(self):
        mock_db = MagicMock()
        mock_result = MagicMock()
        mock_result.mappings.return_value.all.return_value = [
            {
                "event_type": "user_query",
                "token_usage": None,
                "llm_model_used": None,
                "skill_name": None,
                "content": "hello",
            },
        ]
        mock_db.execute.return_value = mock_result

        skill = EvaluateSessionSkill(db=mock_db)
        result = await skill.execute(
            EvaluateSessionInput(target_session_id="test-session", include_details=True)
        )

        assert result.success is True
        assert result.event_breakdown is not None
        assert len(result.event_breakdown) == 1


class TestSkillMetadata:
    """Test skill metadata and schema."""

    def test_skill_attributes(self):
        skill = EvaluateSessionSkill()

        assert skill.name == "evaluate_session"
        assert skill.version == "1.0.0"
        assert skill.description

    def test_requirements(self):
        skill = EvaluateSessionSkill()

        from core.skills.base import RuntimeRequirement, SideEffectCategory

        assert RuntimeRequirement.DATABASE in skill.requirements.runtime
        assert skill.requirements.llm_required is False
        assert skill.side_effect_profile.category == SideEffectCategory.READ

    def test_openai_schema(self):
        skill = EvaluateSessionSkill()
        schema = skill.to_openai_schema()

        assert schema["type"] == "function"
        assert schema["function"]["name"] == "evaluate_session"
        assert "target_session_id" in schema["function"]["parameters"]["properties"]
        assert "target_session_id" in schema["function"]["parameters"]["required"]
