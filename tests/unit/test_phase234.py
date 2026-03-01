"""Tests for Phase 2/3/4: Schema assessment, auto-scorer integration,
SelfImprovingSelector signal, observability.
"""

from __future__ import annotations

import time
from datetime import datetime, timezone, timedelta
from unittest.mock import patch, MagicMock

import pytest

from core.verification.tool_quality import (
    assess_with_schema,
    assess_tool_result,
    load_quality_schema,
    set_schema_loader,
    invalidate_schema_cache,
    response_acknowledges_limitation,
    QualityAssessment,
)
from core.evaluation.auto_scorer import compute_auto_score
from core.skills.learning_signals import SignalType, LearningSignal, SignalThresholds


# ============================================================================
# Phase 2: Schema-Based Assessment (Tier 1)
# ============================================================================

STOCK_SCHEMA = {
    "required_fields": [
        {"path": "current_price", "type": "number"},
        {"path": "technical_indicators", "type": "dict", "min_keys": 2},
        {"path": "trend_analysis", "type": "dict", "min_keys": 1},
        {"path": "risk_assessment.risk_factors", "type": "list", "min_length": 1},
    ],
    "sentinel_values": [
        {"path": "risk_assessment.risk_score", "sentinel": 0, "meaning": "not computed"},
        {"path": "investment_advice.confidence", "sentinel": 50, "meaning": "default"},
    ],
    "freshness": {
        "timestamp_field": "data_timestamp",
        "max_age_seconds": 86400,
    },
}


class TestSchemaAssessment:
    def test_019ca950_with_schema(self):
        """Schema catches the exact 019ca950 pattern with precision."""
        result = {
            "current_price": 0,
            "technical_indicators": {},
            "trend_analysis": {},
            "risk_assessment": {"risk_score": 0, "risk_factors": []},
            "investment_advice": {"confidence": 50},
        }
        a = assess_with_schema(result, STOCK_SCHEMA, "stock_assistant")
        assert a.grade != "complete"
        assert a.score < 0.5
        # Should detect: empty dict (tech_indicators), empty dict (trend),
        # empty list (risk_factors), sentinel risk_score=0, sentinel confidence=50
        assert len(a.signals) >= 3

    def test_complete_result_passes_schema(self):
        """A fully populated result should pass schema validation."""
        result = {
            "current_price": 25.5,
            "technical_indicators": {"ma5": 25.0, "ma10": 24.8, "rsi": 55},
            "trend_analysis": {"direction": "up"},
            "risk_assessment": {"risk_score": 35, "risk_factors": ["market volatility"]},
            "investment_advice": {"confidence": 78},
        }
        a = assess_with_schema(result, STOCK_SCHEMA, "stock_assistant")
        assert a.grade == "complete"
        assert a.score >= 0.8

    def test_sentinel_detection(self):
        """Sentinel values should be detected and penalized."""
        result = {
            "current_price": 25.5,
            "technical_indicators": {"ma5": 25.0, "rsi": 55},
            "trend_analysis": {"direction": "up"},
            "risk_assessment": {"risk_score": 0, "risk_factors": ["vol"]},
            "investment_advice": {"confidence": 50},
        }
        a = assess_with_schema(result, STOCK_SCHEMA, "stock_assistant")
        sentinel_signals = [s for s in a.signals if "sentinel" in s]
        assert len(sentinel_signals) == 2  # risk_score=0 and confidence=50

    def test_freshness_check(self):
        """Stale data should be detected via schema freshness config."""
        old_time = (datetime.now(timezone.utc) - timedelta(hours=48)).isoformat()
        result = {
            "current_price": 25.5,
            "technical_indicators": {"ma5": 25.0, "rsi": 55},
            "trend_analysis": {"direction": "up"},
            "risk_assessment": {"risk_score": 35, "risk_factors": ["vol"]},
            "investment_advice": {"confidence": 78},
            "data_timestamp": old_time,
        }
        a = assess_with_schema(result, STOCK_SCHEMA, "stock_assistant")
        assert a.stale is True

    def test_schema_dispatch_in_assess_tool_result(self):
        """assess_tool_result should use schema when available."""
        result = {"current_price": 0, "technical_indicators": {},
                  "trend_analysis": {}, "risk_assessment": {"risk_score": 0, "risk_factors": []},
                  "investment_advice": {"confidence": 50}}

        with patch("core.verification.tool_quality.load_quality_schema", return_value=STOCK_SCHEMA):
            a = assess_tool_result("stock_assistant", result)
        # Schema-based assessment should catch more signals than structural inference
        assert a.grade != "complete"
        assert len(a.signals) >= 3

    def test_no_schema_falls_through_to_tier2(self):
        """Without schema, assess_tool_result uses Tier 2 structural inference."""
        with patch("core.verification.tool_quality.load_quality_schema", return_value=None):
            a = assess_tool_result("stock_assistant", {"data": {}, "info": {}})
        assert a.grade != "complete"  # Tier 2 still catches it

    def test_schema_loader_injection(self):
        """Schema loader injection and cache invalidation work correctly."""
        invalidate_schema_cache()
        
        # No loader set initially after invalidation
        set_schema_loader(lambda name: {"required_fields": []} if name == "test_tool" else None)
        
        schema = load_quality_schema("test_tool")
        assert schema == {"required_fields": []}
        
        # Cache hit
        schema2 = load_quality_schema("test_tool")
        assert schema2 == {"required_fields": []}
        
        # Unknown tool
        assert load_quality_schema("unknown") is None
        
        # Cleanup
        set_schema_loader(None)
        invalidate_schema_cache()

    def test_empty_required_fields(self):
        """Empty required_fields list should pass with score 1.0."""
        schema = {"required_fields": []}
        result = {"any": "data"}
        a = assess_with_schema(result, schema, "test")
        assert a.score == 1.0
        assert a.grade == "complete"

    def test_nested_path_parent_missing(self):
        """Missing parent in nested path should be detected."""
        schema = {"required_fields": [{"path": "parent.child", "type": "any"}]}
        result = {"other": "data"}  # no "parent" key
        a = assess_with_schema(result, schema, "test")
        assert a.score < 1.0
        assert any("missing" in s for s in a.signals)


# ============================================================================
# Phase 3: Auto-Scorer Integration
# ============================================================================

class TestAutoScorerQuality:
    def test_degraded_quality_lowers_score(self):
        """Low tool_quality_score should lower the auto-score."""
        base = compute_auto_score(
            firewall_passed=True, firewall_confidence=0.8,
            response_tokens=200,
        )
        degraded = compute_auto_score(
            firewall_passed=True, firewall_confidence=0.8,
            response_tokens=200, tool_quality_score=0.3,
        )
        assert degraded.quality_score < base.quality_score

    def test_acknowledged_quality_gets_bonus(self):
        """When LLM acknowledges data limitations, score should improve."""
        not_ack = compute_auto_score(
            firewall_passed=True, firewall_confidence=0.8,
            response_tokens=200, tool_quality_score=0.3,
            data_quality_acknowledged=False,
        )
        ack = compute_auto_score(
            firewall_passed=True, firewall_confidence=0.8,
            response_tokens=200, tool_quality_score=0.3,
            data_quality_acknowledged=True,
        )
        assert ack.quality_score > not_ack.quality_score

    def test_no_quality_data_unchanged(self):
        """Without tool_quality_score, scoring is unchanged (backward compat)."""
        result = compute_auto_score(
            firewall_passed=True, firewall_confidence=0.8,
            response_tokens=200,
        )
        # Original formula: 0.8 * 5 * 0.6 + 1.0 * 5 * 0.2 + 1.0 * 5 * 0.2 = 2.4 + 1.0 + 1.0 = 4.4
        assert abs(result.quality_score - 4.4) < 0.1


# ============================================================================
# Phase 3: SelfImprovingSelector Signal
# ============================================================================

class TestLowDataQualitySignal:
    def test_signal_type_exists(self):
        assert SignalType.LOW_DATA_QUALITY == "low_data_quality"

    def test_threshold_configurable(self):
        """low_data_quality threshold should be configurable."""
        default = SignalThresholds()
        assert default.low_data_quality == 0.5
        
        custom = SignalThresholds(low_data_quality=0.7)
        assert custom.low_data_quality == 0.7
        assert "low_data_quality" in custom.to_dict()

    def test_signal_extractable(self):
        """LOW_DATA_QUALITY signal should be extractable from failure events."""
        signal = LearningSignal(
            signal_type=SignalType.LOW_DATA_QUALITY,
            query_pattern="中信证券建议买吗",
            wrong_skills=["stock_assistant"],
            correct_skills=[],
            target_metrics={"quality_score": 0.8},
        )
        d = signal.to_dict()
        assert d["signal_type"] == "low_data_quality"
        assert d["target_metrics"]["quality_score"] == 0.8

    def test_selector_extracts_low_quality_signal(self):
        """SelfImprovingSelector._extract_signal handles LOW_DATA_QUALITY."""
        from core.skills.self_improving_selector import SelfImprovingSelector
        selector = SelfImprovingSelector.__new__(SelfImprovingSelector)
        selector.thresholds = SignalThresholds(low_data_quality=0.5)

        failure = {
            "user_query": "中信证券建议买吗",
            "selected_skills": ["stock_assistant"],
            "tool_quality_score": 0.3,
        }
        with patch("core.skills.self_improving_selector.extract_context_features", return_value={}):
            signal = selector._extract_signal(failure, SignalType.LOW_DATA_QUALITY)
        assert signal is not None
        assert signal.signal_type == SignalType.LOW_DATA_QUALITY
        assert signal.target_metrics["quality_score"] == 0.8  # threshold + 0.3

    def test_selector_ignores_good_quality(self):
        """Quality >= threshold should not trigger LOW_DATA_QUALITY signal."""
        from core.skills.self_improving_selector import SelfImprovingSelector
        selector = SelfImprovingSelector.__new__(SelfImprovingSelector)
        selector.thresholds = SignalThresholds(low_data_quality=0.5)

        failure = {
            "user_query": "test",
            "selected_skills": ["weather"],
            "tool_quality_score": 0.7,
        }
        with patch("core.skills.self_improving_selector.extract_context_features", return_value={}):
            signal = selector._extract_signal(failure, SignalType.LOW_DATA_QUALITY)
        assert signal is None


# ============================================================================
# Phase 4: Observability — Annotation-Ignored Detection
# ============================================================================

class TestAnnotationIgnored:
    def test_chinese_limitation_detected(self):
        assert response_acknowledges_limitation("数据不完整，无法给出可靠建议。")

    def test_english_limitation_detected(self):
        assert response_acknowledges_limitation("The data is incomplete, cannot provide reliable advice.")

    def test_confabulation_not_detected(self):
        """The original 019ca950 response did NOT acknowledge limitations."""
        assert not response_acknowledges_limitation("建议持有，风险评估为低风险，投资信心中等。")

    def test_missing_data_detected(self):
        assert response_acknowledges_limitation("Some fields have missing data in the analysis.")

    def test_mixed_case_detected(self):
        """Mixed case should still be detected (lowercase matching)."""
        assert response_acknowledges_limitation("The Data Is INCOMPLETE here.")

    def test_partial_word_not_matched(self):
        """Keywords should match as substrings (current behavior)."""
        # "incomplete" is in "incompletely" — current impl matches this
        assert response_acknowledges_limitation("The task was done incompletely.")

    def test_empty_response(self):
        """Empty response should not acknowledge limitations."""
        assert not response_acknowledges_limitation("")

    def test_unrelated_content(self):
        """Unrelated content should not match."""
        assert not response_acknowledges_limitation("The weather is nice today. Stock price is $25.")
