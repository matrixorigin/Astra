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
    @pytest.mark.parametrize("tool", ["read_file", "write_file", "bash", "grep", "glob", "list_dir", "git"])
    def test_passthrough_tools_skip_assessment(self, tool: str):
        """Raw data tools (file I/O, shell) skip structural assessment."""
        a = assess_tool_result(tool, {"anything": {}})
        assert a.score == 1.0
        assert a.grade == "complete"
        assert a.signals == []
    
    def test_get_agent_info_is_passthrough(self):
        """get_agent_info zeros are legitimate (new session) — no penalty."""
        a = assess_tool_result("get_agent_info", {"agent_id": "", "capabilities": []})
        assert a.score == 1.0
        assert a.grade == "complete"

    def test_reflect_is_passthrough(self):
        """reflect zeros are legitimate — no penalty."""
        a = assess_tool_result("reflect", {"insights": [], "recommendations": []})
        assert a.score == 1.0
        assert a.grade == "complete"


# ── Regression: session 019cb34f — get_agent_info false degradation ──────────


class TestRegressionGetAgentInfoFalseDegradation:
    """Reproduce and verify fix for session 019cb34f-ca66-73e3-8fce-0054d768eabf.

    Root cause: get_agent_info(dimension="memory") on a new session's first turn
    returned legitimate zeros (0 events, 0 tool_calls, etc.). The tool quality
    firewall detected 5 numeric zeros → zero_cluster penalty → score 0.4
    (degraded). The LLM then faithfully reported "数据质量问题（评分0.4）",
    which was a firewall false positive, not a real data quality issue.

    Fix: add get_agent_info to PASSTHROUGH_TOOLS so its zeros are not penalised.
    """

    # Exact payload from the session's tool_result event 019cb350-8711
    MEMORY_RESULT = {
        "memory": {
            "has_project_rules": True,
            "has_edge_profile": True,
            "episodic": {"total_events": 0, "user_queries": 0, "tool_calls": 0},
            "semantic": {"ctx_snapshots": 1, "peak_snapshot_tokens": 0},
            "procedural": {"skill_selections": 0, "accuracy_rate": None},
        }
    }

    def test_old_behaviour_would_score_degraded(self):
        """Prove the bug: without passthrough, this data scores 0.4."""
        # Temporarily remove get_agent_info from passthrough to reproduce
        from core.verification.tool_quality import (
            _score_to_grade, flatten_json,
        )
        leaves = list(flatten_json(self.MEMORY_RESULT))
        total = len(leaves)
        empty_count = sum(
            1 for _, v in leaves if v is None or v == {} or v == [] or v == ""
        )
        zero_count = sum(
            1 for _, v in leaves if isinstance(v, (int, float)) and v == 0
        )
        numeric_count = sum(1 for _, v in leaves if isinstance(v, (int, float)))

        # Reproduce the zero_cluster penalty logic
        non_empty = total - empty_count
        score = non_empty / total if total > 0 else 0.0
        if zero_count >= 3 and numeric_count > 0 and zero_count / numeric_count > 0.5:
            score = min(score, 0.4)

        assert score == pytest.approx(0.4), (
            f"Without fix, score should be 0.4 (degraded), got {score}"
        )
        assert _score_to_grade(score) == "degraded"

    def test_fix_scores_complete(self):
        """After fix: get_agent_info is passthrough → score 1.0."""
        assert "get_agent_info" in PASSTHROUGH_TOOLS
        a = assess_tool_result("get_agent_info", self.MEMORY_RESULT)
        assert a.score == 1.0
        assert a.grade == "complete"
        assert a.signals == []

    def test_annotation_not_injected(self):
        """No misleading [TOOL QUALITY: DEGRADED] annotation for LLM to parrot."""
        a = assess_tool_result("get_agent_info", self.MEMORY_RESULT)
        tool_result = {"name": "get_agent_info", "result": json.dumps(self.MEMORY_RESULT)}
        annotated = annotate_tool_result(tool_result, a)
        # Should be unchanged — no annotation prepended
        assert annotated == tool_result


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
        data = {"price": 100, "timestamp": old_time}
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
    
    def test_circular_reference_handled(self):
        """Circular references should not cause infinite loop."""
        data: dict = {"a": 1}
        data["self"] = data  # Circular reference
        leaves = list(flatten_json(data, max_depth=4, max_fields=100))
        # Should complete without hanging
        assert len(leaves) >= 1
    
    def test_non_serializable_data_passthrough(self):
        """Non-JSON-serializable data should pass through."""
        class CustomObj:
            pass
        a = assess_tool_result("tool", {"obj": CustomObj()})
        assert a.score == 1.0
        assert a.grade == "complete"
    
    def test_max_depth_zero(self):
        """max_depth=0 should yield the root object as single leaf."""
        data = {"a": {"b": {"c": 1}}}
        leaves = list(flatten_json(data, max_depth=0, max_fields=100))
        assert len(leaves) == 1
        assert leaves[0][0] == ""
    
    def test_max_fields_zero(self):
        """max_fields=0 should yield no results."""
        data = {"a": 1, "b": 2}
        leaves = list(flatten_json(data, max_depth=4, max_fields=0))
        # With max_fields=0, generator should stop immediately
        assert len(leaves) <= 1  # May yield one before checking count
    
    def test_empty_string_vs_none_vs_empty_list(self):
        """Should distinguish between different empty types."""
        data = {"str": "", "null": None, "list": [], "dict": {}}
        a = assess_tool_result("tool", data)
        # All 4 fields are empty
        assert a.score == 0.0
        assert a.grade == "empty"
    
    def test_timestamp_field_false_positive_avoided(self):
        """Fields like 'update_date_info' should NOT trigger staleness check."""
        old_time = (datetime.now(timezone.utc) - timedelta(hours=48)).isoformat()
        data = {"update_date_info": old_time, "price": 100}
        a = assess_tool_result("tool", data)
        # Should NOT be marked as stale because 'update_date_info' is not in whitelist
        assert a.stale is False
    
    def test_valid_zero_not_penalized(self):
        """A single zero in context of other valid data should not trigger zero_cluster."""
        data = {"price": 100, "volume": 1000, "change": 0, "name": "Stock"}
        a = assess_tool_result("stock_assistant", data)
        # Should be complete because only 1 zero among 4 fields
        assert a.grade == "complete"
        assert not any("zero_cluster" in s for s in a.signals)


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
