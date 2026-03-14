"""Unit tests for SemanticDiff — pure comparison functions + input validation."""

import pytest
from types import SimpleNamespace

from core.replay.semantic_diff import SemanticDiff, _validate_name


def _event(
    event_type="user_query",
    token_total=0,
    token_prompt=0,
    token_completion=0,
    causal_chain_id=None,
    quality_score=None,
):
    """Create a minimal mock event for testing pure comparison functions."""
    token_usage = None
    if token_total > 0:
        token_usage = SimpleNamespace(
            total=token_total, prompt=token_prompt, completion=token_completion
        )
    return SimpleNamespace(
        event_type=event_type,
        token_usage=token_usage,
        causal_chain_id=causal_chain_id,
        quality_score=quality_score,
    )


class TestValidateName:
    def test_accepts_valid_names(self):
        for name in ["checkpoint1", "snap_2026-02-26", "a-b-c", "ABC_123"]:
            _validate_name(name)  # should not raise

    def test_rejects_sql_injection(self):
        for bad in ["'; DROP TABLE --", "a b", "foo'bar", "x;y", "a\nb"]:
            with pytest.raises(ValueError, match="Invalid"):
                _validate_name(bad)


class TestCompareTokenUsage:
    def test_basic_diff(self):
        e1 = [_event(token_total=100, token_prompt=60, token_completion=40)]
        e2 = [_event(token_total=150, token_prompt=90, token_completion=60)]
        result = SemanticDiff._compare_token_usage(e1, e2)
        assert result["total"]["diff"] == 50
        assert result["prompt"]["diff"] == 30
        assert result["completion"]["diff"] == 20
        assert "50.0%" in result["efficiency_change"]

    def test_empty_events(self):
        result = SemanticDiff._compare_token_usage([], [])
        assert result["total"]["diff"] == 0
        assert result["efficiency_change"] == "N/A"

    def test_no_token_usage(self):
        e1 = [_event()]
        e2 = [_event()]
        result = SemanticDiff._compare_token_usage(e1, e2)
        assert result["total"]["session1"] == 0


class TestCompareDecisionPaths:
    def test_different_chain_counts(self):
        e1 = [_event(causal_chain_id="c1"), _event(causal_chain_id="c1")]
        e2 = [_event(causal_chain_id="c2"), _event(causal_chain_id="c3")]
        result = SemanticDiff._compare_decision_paths(e1, e2)
        assert result["chain_count"]["session1"] == 1
        assert result["chain_count"]["session2"] == 2
        assert result["chain_count"]["diff"] == 1

    def test_empty_chains(self):
        result = SemanticDiff._compare_decision_paths([], [])
        assert result["chain_count"]["diff"] == 0
        assert result["avg_chain_length"]["diff"] == 0

    def test_complexity_change(self):
        e1 = [_event(causal_chain_id="c1")]
        e2 = [
            _event(causal_chain_id="c2"),
            _event(causal_chain_id="c2"),
            _event(causal_chain_id="c2"),
        ]
        result = SemanticDiff._compare_decision_paths(e1, e2)
        assert result["complexity_change"] == "increased"


class TestCompareEventTypes:
    def test_type_distribution(self):
        e1 = [_event("user_query"), _event("user_query"), _event("llm_response")]
        e2 = [_event("user_query"), _event("llm_response"), _event("tool_call")]
        result = SemanticDiff._compare_event_types(e1, e2)
        assert result["user_query"]["diff"] == -1
        assert result["tool_call"]["session1"] == 0
        assert result["tool_call"]["session2"] == 1


class TestCompareQuality:
    def test_quality_improvement(self):
        e1 = [_event(quality_score=3.0), _event(quality_score=3.0)]
        e2 = [_event(quality_score=4.0), _event(quality_score=5.0)]
        result = SemanticDiff._compare_quality(e1, e2)
        assert result["avg_quality"]["diff"] == 1.5
        assert result["quality_change"] == "improved"

    def test_no_scores(self):
        result = SemanticDiff._compare_quality([], [])
        assert result["avg_quality"]["diff"] == 0


class TestGenerateSummary:
    def test_more_tokens(self):
        token_diff = {"total": {"diff": 100}}
        path_diff = {"chain_count": {"diff": 0}}
        assert "100 more tokens" in SemanticDiff._generate_summary(token_diff, path_diff, {})

    def test_saved_tokens(self):
        token_diff = {"total": {"diff": -50}}
        path_diff = {"chain_count": {"diff": 0}}
        assert "Saved 50 tokens" in SemanticDiff._generate_summary(token_diff, path_diff, {})

    def test_no_changes(self):
        token_diff = {"total": {"diff": 0}}
        path_diff = {"chain_count": {"diff": 0}}
        assert SemanticDiff._generate_summary(token_diff, path_diff, {}) == "No significant changes"

    def test_combined(self):
        token_diff = {"total": {"diff": 200}}
        path_diff = {"chain_count": {"diff": -2}}
        summary = SemanticDiff._generate_summary(token_diff, path_diff, {})
        assert "200 more tokens" in summary
        assert "2 fewer decision chains" in summary
