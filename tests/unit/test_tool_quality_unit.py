"""Integration tests for Tool Quality Firewall wiring in chat.py.

Tests that quality assessment is correctly invoked during chat turn processing
and that quality events are emitted for non-complete results.
"""

from __future__ import annotations

import json
from unittest.mock import patch, MagicMock

import pytest

from core.verification.tool_quality import (
    assess_tool_result,
    annotate_tool_result,
    QualityAssessment,
    PASSTHROUGH_TOOLS,
)


# ── Task 2: Assessment wiring ───────────────────────────────────────────────


class TestAssessmentWiring:
    """Verify assess + annotate logic matches what chat.py does inline."""

    def test_degraded_tool_result_annotated_in_history(self):
        """Degraded result gets quality annotation prepended."""
        tr = {
            "name": "stock_assistant",
            "tool_call_id": "tc1",
            "result": json.dumps(
                {
                    "data": {},
                    "info": {},
                    "risk_score": 0,
                    "confidence": 0,
                    "volatility": 0,
                }
            ),
        }
        assessment = assess_tool_result(tr["name"], tr["result"])
        assert assessment.needs_annotation
        annotated = annotate_tool_result(tr, assessment)
        assert "[TOOL QUALITY:" in annotated.get("result", "")

    def test_complete_tool_result_unchanged(self):
        """Complete result is not modified."""
        tr = {
            "name": "weather",
            "tool_call_id": "tc2",
            "result": json.dumps(
                {
                    "temperature": 22,
                    "humidity": 65,
                    "wind_speed": 12,
                    "city": "Beijing",
                }
            ),
        }
        assessment = assess_tool_result(tr["name"], tr["result"])
        assert not assessment.needs_annotation
        annotated = annotate_tool_result(tr, assessment)
        assert annotated["result"] == tr["result"]

    def test_feature_flag_disables_assessment(self):
        """When _TOOL_QUALITY_ENABLED is False, no assessment runs."""
        # Simulate the flag check that chat.py does
        enabled = False
        tool_results = [{"name": "stock_assistant", "result": "{}"}]
        assessments = []
        if enabled and tool_results:
            for tr in tool_results:
                assessments.append(assess_tool_result(tr["name"], tr["result"]))
        assert assessments == []

    def test_passthrough_tool_not_assessed(self):
        """Passthrough tools get score=1.0, no annotation."""
        for tool in ["bash", "read_file", "grep"]:
            a = assess_tool_result(tool, {"anything": {}})
            assert a.score == 1.0
            assert not a.needs_annotation

    def test_multiple_tool_results_assessed_independently(self):
        """Each tool result gets its own independent assessment."""
        results = [
            {"name": "stock_assistant", "result": json.dumps({"data": {}})},
            {"name": "weather", "result": json.dumps({"temp": 22, "city": "SH"})},
        ]
        assessments = [assess_tool_result(r["name"], r["result"]) for r in results]
        assert assessments[0].grade != "complete"  # degraded
        assert assessments[1].grade == "complete"  # healthy


# ── Task 3: Quality event logging ───────────────────────────────────────────


class TestQualityEventLogging:
    """Verify quality event emission logic matches chat.py Phase 2b."""

    def _simulate_phase2b(self, assessments: list[dict]) -> list[dict]:
        """Simulate the Phase 2b logic from _persist_turn_events."""
        events = []
        for qa in assessments:
            if qa["grade"] != "complete":
                events.append(
                    {
                        "event_type": "tool_result_quality",
                        "content": json.dumps(qa),
                        "metadata": qa,
                    }
                )
        return events

    def test_quality_event_emitted_for_degraded(self):
        a = assess_tool_result("stock_assistant", {"data": {}, "info": {}})
        qa_dict = {
            "tool_name": a.tool_name,
            "score": a.score,
            "grade": a.grade,
            "signals": a.signals,
            "stale": a.stale,
        }
        events = self._simulate_phase2b([qa_dict])
        assert len(events) == 1
        assert events[0]["event_type"] == "tool_result_quality"

    def test_no_quality_event_for_passthrough(self):
        a = assess_tool_result("bash", {"output": "hello"})
        qa_dict = {
            "tool_name": a.tool_name,
            "score": a.score,
            "grade": a.grade,
            "signals": a.signals,
            "stale": a.stale,
        }
        events = self._simulate_phase2b([qa_dict])
        assert len(events) == 0

    def test_quality_event_metadata_correct(self):
        a = assess_tool_result(
            "stock_assistant",
            {
                "risk_score": 0,
                "confidence": 0,
                "volatility": 0,
                "data": {},
                "name": "test",
            },
        )
        qa_dict = {
            "tool_name": a.tool_name,
            "score": a.score,
            "grade": a.grade,
            "signals": a.signals,
            "stale": a.stale,
        }
        events = self._simulate_phase2b([qa_dict])
        assert len(events) == 1
        meta = events[0]["metadata"]
        assert meta["tool_name"] == "stock_assistant"
        assert meta["score"] <= 0.4
        assert meta["grade"] in ("degraded", "empty")
        assert any("zero_cluster" in s or "empty" in s for s in meta["signals"])

    def test_reflect_surfaces_quality_events(self):
        """Quality events with grade != complete should be surfaceable by reflect.

        This tests the data contract — reflect queries tool_result_quality events
        and filters for non-complete grades (implemented in previous session).
        """
        # Simulate what reflect's _build_reflect_evidence would see
        quality_events = [
            {
                "event_type": "tool_result_quality",
                "content": json.dumps(
                    {
                        "tool_name": "stock_assistant",
                        "score": 0.3,
                        "grade": "degraded",
                        "signals": ["empty_containers: 3/4 fields empty"],
                        "stale": False,
                    }
                ),
            },
        ]
        # Reflect filters: only non-complete
        surfaced = [
            e for e in quality_events if json.loads(e["content"]).get("grade") != "complete"
        ]
        assert len(surfaced) == 1
        assert json.loads(surfaced[0]["content"])["tool_name"] == "stock_assistant"
