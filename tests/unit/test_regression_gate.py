"""Tests for regression gate."""

import json
from unittest.mock import Mock
import uuid

import pytest

from core.skills.regression_gate import SkillSelectionRegressionGate


@pytest.fixture
def mock_llm():
    llm = Mock()
    llm.chat_with_tools = Mock(return_value={"tool_calls": []})
    return llm


@pytest.fixture
def gate(db, mock_llm):
    return SkillSelectionRegressionGate(mock_llm, db)


class TestSkillSelectionRegressionGate:

    def test_validate_empty_queries(self, gate):
        mock_sel = Mock()
        result = gate.validate_selector_change(
            new_selector=mock_sel, old_selector=mock_sel, test_queries=[],
        )
        assert result["verdict"] in ["pass", "fail"]
        assert result["test_count"] == 0

    def test_validate_with_queries(self, gate):
        """Gate calls get_tools_schema on both selectors."""
        new_sel = Mock()
        old_sel = Mock()
        new_sel.get_tools_schema = Mock(return_value=Mock(tools=[{"fn": "a"}]))
        old_sel.get_tools_schema = Mock(return_value=Mock(tools=[]))

        result = gate.validate_selector_change(
            new_selector=new_sel, old_selector=old_sel,
            test_queries=["q1", "q2"],
        )
        assert result["test_count"] == 2
        assert result["new_avg_score"] >= result["old_avg_score"]

    def test_validate_handles_errors(self, gate):
        sel = Mock()
        sel.get_tools_schema = Mock(side_effect=RuntimeError("boom"))
        result = gate.validate_selector_change(
            new_selector=sel, old_selector=sel, test_queries=["q"],
        )
        assert result["test_count"] == 1
        assert result["new_avg_score"] == 0.0

    def test_get_gate_stats_no_data(self, gate, db):
        from sqlalchemy import text
        db.execute(text("DELETE FROM selector_gate_results"))
        db.commit()
        stats = gate.get_gate_stats()
        assert stats["total_gates"] == 0

    def test_get_gate_stats_with_results(self, gate, db):
        from sqlalchemy import text
        db.execute(text("DELETE FROM selector_gate_results"))
        db.commit()

        for i in range(5):
            verdict = "PASS" if i < 3 else "FAIL"
            db.execute(text("""
                INSERT INTO selector_gate_results
                (gate_id, selector_version, test_count,
                 new_avg_score, old_avg_score,
                 improvement_pct, verdict, details)
                VALUES (:gid, :ver, :cnt, :ns, :os, :imp, :v, :d)
            """), {
                "gid": f"gate-{uuid.uuid4().hex[:8]}",
                "ver": f"v{i}", "cnt": 10,
                "ns": 0.9 if verdict == "PASS" else 0.5,
                "os": 0.8,
                "imp": 10.0 if verdict == "PASS" else -20.0,
                "v": verdict, "d": json.dumps({}),
            })
        db.commit()

        stats = gate.get_gate_stats()
        assert stats["total_gates"] == 5
        assert stats["passed"] == 3
        assert stats["failed"] == 2

    def test_get_golden_queries(self, gate):
        assert isinstance(gate.get_golden_queries(), list)

    def test_get_gate_history(self, gate):
        assert isinstance(gate.get_gate_history(), list)
