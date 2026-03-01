"""End-to-end validation for Tool Result Quality Firewall.

Tests the complete pipeline: assess → annotate → quality event → reflect visibility.
"""

from __future__ import annotations

import json

import pytest

from core.verification.tool_quality import (
    assess_tool_result,
    annotate_tool_result,
)


class TestE2EPipeline:
    def test_019ca950_full_pipeline(self):
        """Reproduce the exact 019ca950 failure: stock_assistant returns
        structurally valid JSON with semantically empty fields.

        Pipeline: assess → detect degraded → annotate → quality event emittable → reflect surfaceable.
        """
        # Exact data from session 019ca950
        tool_result = {
            "name": "stock_assistant",
            "tool_call_id": "call_019ca950",
            "result": json.dumps({
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
            }),
        }

        # Step 1: Assess
        assessment = assess_tool_result(tool_result["name"], tool_result["result"])
        assert assessment.grade != "complete"
        assert assessment.needs_annotation

        # Step 2: Annotate
        annotated = annotate_tool_result(tool_result, assessment)
        assert "[TOOL QUALITY:" in annotated["result"]
        assert "Respond honestly" in annotated["result"]

        # Step 3: Quality event would be emitted (simulate Phase 2b)
        qa_dict = {
            "tool_name": assessment.tool_name,
            "score": assessment.score,
            "grade": assessment.grade,
            "signals": assessment.signals,
            "stale": assessment.stale,
        }
        assert qa_dict["grade"] != "complete"  # would trigger event emission

        # Step 4: Reflect would surface it (simulate reflect filter)
        quality_event = {"event_type": "tool_result_quality", "content": json.dumps(qa_dict)}
        parsed = json.loads(quality_event["content"])
        assert parsed["grade"] != "complete"  # reflect would include this

    def test_healthy_tool_result_no_side_effects(self):
        """A complete tool result should pass through with zero modifications."""
        tool_result = {
            "name": "weather",
            "tool_call_id": "call_healthy",
            "result": json.dumps({
                "temperature": 22.5,
                "humidity": 65,
                "wind_speed": 12,
                "city": "Beijing",
                "forecast": "sunny",
            }),
        }
        original_result = tool_result["result"]

        # Step 1: Assess
        assessment = assess_tool_result(tool_result["name"], tool_result["result"])
        assert assessment.grade == "complete"
        assert not assessment.needs_annotation

        # Step 2: Annotate (should be no-op)
        annotated = annotate_tool_result(tool_result, assessment)
        assert annotated["result"] == original_result

        # Step 3: No quality event (grade == complete)
        qa_dict = {"grade": assessment.grade}
        assert qa_dict["grade"] == "complete"  # Phase 2b would skip
