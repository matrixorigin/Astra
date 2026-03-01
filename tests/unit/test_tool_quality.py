"""Unit tests for core/verification/tool_quality.py — Tool Result Quality Firewall."""

from __future__ import annotations

import json
from datetime import datetime, timezone, timedelta

import pytest

from core.verification.tool_quality import (
    QualityAssessment,
    assess_tool_result,
    annotate_tool_result,
    flatten_json,
    PASSTHROUGH_TOOLS,
)


# ── Tier 3: pass-through ────────────────────────────────────────────────────

class TestPassthrough:
    @pytest.mark.parametrize("tool", sorted(PASSTHROUGH_TOOLS))
    def test_passthrough_tools_skip_assessment(self, tool: str):
        a = assess_tool_result(tool, {"anything": {}})
        assert a.score == 1.0
        assert a.grade == "complete"
        assert a.signals == []


# ── Tier 2: structural inference ─────────────────────────────────────────────

class TestStructuralInference:
    def test_empty_dict_detected(self):
        a = assess_tool_result("stock_assistant", {"data": {}, "info": {}})
        assert a.grade in ("empty", "degraded")
        assert a.score < 0.8
        assert any("empty" in s for s in a.signals)

    def test_empty_list_detected(self):
        a = assess_tool_result("stock_assistant", {"items": [], "tags": []})
        assert a.score < 0.8

    def test_null_cluster_detected(self):
        data = {f"field_{i}": None for i in range(6)}
        data["ok"] = "yes"
        a = assess_tool_result("analyzer", data)
        assert any("null_cluster" in s for s in a.signals)

    def test_zero_cluster_detected(self):
        data = {
            "risk_score": 0, "confidence": 0, "volatility": 0,
            "name": "test",
        }
        a = assess_tool_result("stock_assistant", data)
        assert any("zero_cluster" in s for s in a.signals)
        assert a.score <= 0.4

    def test_explicit_error_returns_empty(self):
        a = assess_tool_result("api_tool", {"error": "timeout", "success": False})
        assert a.score == 0.0
        assert a.grade == "empty"
        assert any("explicit_error" in s for s in a.signals)

    def test_string_result_passthrough(self):
        a = assess_tool_result("some_tool", "plain text result")
        assert a.score == 1.0
        assert a.grade == "complete"

    def test_large_result_skipped(self):
        big = {"k" * 100: "v" * 100 for i in range(500)}
        a = assess_tool_result("big_tool", json.dumps(big))
        assert a.score == 1.0

    def test_freshness_check(self):
        old_time = (datetime.now(timezone.utc) - timedelta(hours=48)).isoformat()
        data = {"price": 100, "data_timestamp": old_time}
        a = assess_tool_result("stock_assistant", data)
        assert a.stale is True
        assert any("stale_data" in s for s in a.signals)

    def test_depth_limit_respected(self):
        # Build deeply nested dict — should not recurse forever
        nested: dict = {"leaf": "value"}
        for _ in range(10):
            nested = {"level": nested}
        leaves = list(flatten_json(nested, max_depth=4, max_fields=100))
        # Should stop at depth 4, yielding the remaining subtree as a single leaf
        assert len(leaves) <= 100


# ── Annotation ───────────────────────────────────────────────────────────────

class TestAnnotation:
    def test_complete_result_no_annotation(self):
        a = QualityAssessment(tool_name="t", score=1.0, grade="complete")
        original = {"content": "good data"}
        result = annotate_tool_result(original, a)
        assert result["content"] == "good data"

    def test_degraded_result_gets_annotation(self):
        a = QualityAssessment(
            tool_name="t", score=0.3, grade="degraded",
            signals=["empty_containers: 3/4 fields empty"],
        )
        original = {"content": '{"data": {}}'}
        result = annotate_tool_result(original, a)
        assert "[TOOL QUALITY: DEGRADED" in result["content"]
        assert "empty_containers" in result["content"]
        assert "Respond honestly" in result["content"]

    def test_annotation_capped_at_5_signals(self):
        a = QualityAssessment(
            tool_name="t", score=0.2, grade="degraded",
            signals=[f"signal_{i}" for i in range(5)],
        )
        original = {"result": "data"}
        result = annotate_tool_result(original, a)
        # All 5 signals should appear (cap is 5)
        for i in range(5):
            assert f"signal_{i}" in result["result"]


# ── 019ca950 regression test ─────────────────────────────────────────────────

class TestRegressionCase:
    def test_stock_assistant_019ca950(self):
        """Exact data pattern from session 019ca950 that caused confabulation."""
        result = {
            "stock_code": "600030",
            "stock_name": "中信证券",
            "current_price": 0,
            "price_change": 0,
            "technical_indicators": {},
            "trend_analysis": {},
            "risk_score": 0,
            "risk_factors": [],
            "recommendation": "",
            "confidence": 50,
        }
        a = assess_tool_result("stock_assistant", result)
        # Must detect this as non-complete
        assert a.grade != "complete", f"019ca950 pattern must not pass as complete: {a}"
        assert a.score < 0.8
        assert a.needs_annotation is True
        # Should detect empty containers and zero cluster
        signal_text = " ".join(a.signals)
        assert "empty" in signal_text or "zero" in signal_text
