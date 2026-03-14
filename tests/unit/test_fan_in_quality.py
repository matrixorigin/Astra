"""Tests for multi-agent fan-in quality assessment and conflict detection.

Covers:
- AggregatedResult quality metrics (success_rate, counts)
- Conflict detection between agents referencing same artifacts
- Summary backward compatibility
- Edge cases: empty results, all failures, no conflicts
"""

import pytest
from core.agent.coordination import (
    AggregatedResult,
    Conflict,
    CoordinationPatterns,
    Result,
    detect_conflicts,
    _extract_artifacts,
)
from unittest.mock import Mock


class TestExtractArtifacts:
    def test_extracts_file_paths(self):
        text = "Found issue in `auth.py` and `login.py`"
        arts = _extract_artifacts(text)
        assert "auth.py" in arts
        assert "login.py" in arts

    def test_extracts_function_names(self):
        text = "Refactor function validate() and check_auth()"
        arts = _extract_artifacts(text)
        assert "validate()" in arts
        assert "check_auth()" in arts

    def test_ignores_short_tokens(self):
        text = "Fix a in b.py"
        arts = _extract_artifacts(text)
        assert "a" not in arts  # too short
        assert "b.py" in arts

    def test_empty_text(self):
        assert _extract_artifacts("") == set()

    def test_rejects_plain_words(self):
        """Plain words without artifact structure are NOT extracted."""
        text = "The error was critical and we should fix it"
        arts = _extract_artifacts(text)
        assert arts == set()

    def test_extracts_path_with_directory(self):
        text = "Modified core/utils.py and tests/test_auth.py"
        arts = _extract_artifacts(text)
        assert "core/utils.py" in arts
        assert "tests/test_auth.py" in arts


class TestDetectConflicts:
    def test_no_conflict_single_result(self):
        results = [Result(agent_id="a", success=True, output="Fix auth.py")]
        assert detect_conflicts(results) == []

    def test_no_conflict_different_artifacts(self):
        results = [
            Result(agent_id="code", success=True, output="Fix auth.py"),
            Result(agent_id="perf", success=True, output="Optimize cache.py"),
        ]
        assert detect_conflicts(results) == []

    def test_conflict_same_artifact(self):
        results = [
            Result(agent_id="code", success=True, output="Refactor auth.py: extract method"),
            Result(agent_id="security", success=True, output="Rewrite auth.py: fix vulnerability"),
        ]
        conflicts = detect_conflicts(results)
        assert len(conflicts) >= 1
        auth_conflict = [c for c in conflicts if c.artifact == "auth.py"]
        assert len(auth_conflict) == 1
        assert set(auth_conflict[0].agents) == {"code", "security"}

    def test_failed_results_excluded(self):
        results = [
            Result(agent_id="code", success=True, output="Fix auth.py"),
            Result(agent_id="security", success=False, output="", error="timeout"),
        ]
        # Only 1 successful result → no conflict possible
        assert detect_conflicts(results) == []

    def test_three_way_conflict(self):
        results = [
            Result(agent_id="code", success=True, output="Modify validate() to add logging"),
            Result(agent_id="perf", success=True, output="Don't touch validate(), it's hot-path"),
            Result(
                agent_id="security",
                success=True,
                output="Rewrite validate() for input sanitization",
            ),
        ]
        conflicts = detect_conflicts(results)
        validate_conflicts = [c for c in conflicts if "validate()" in c.artifact]
        assert len(validate_conflicts) == 1
        assert len(validate_conflicts[0].agents) == 3

    def test_conflicts_sorted_by_artifact(self):
        """Conflicts are returned in deterministic order (sorted by artifact)."""
        results = [
            Result(agent_id="a", success=True, output="Fix zebra.py and alpha.py"),
            Result(agent_id="b", success=True, output="Rewrite zebra.py and alpha.py"),
        ]
        conflicts = detect_conflicts(results)
        assert len(conflicts) >= 2
        artifacts = [c.artifact for c in conflicts]
        assert artifacts == sorted(artifacts)


class TestFanIn:
    def _make_coord(self):
        return CoordinationPatterns(delegation_skill=Mock())

    def test_returns_aggregated_result(self):
        coord = self._make_coord()
        results = [
            Result(agent_id="a", success=True, output="done"),
            Result(agent_id="b", success=False, output="", error="fail"),
        ]
        agg = coord.fan_in(results)
        assert isinstance(agg, AggregatedResult)
        assert agg.total == 2
        assert agg.succeeded == 1
        assert agg.failed == 1
        assert agg.success_rate == 0.5

    def test_all_success(self):
        coord = self._make_coord()
        results = [
            Result(agent_id="a", success=True, output="ok"),
            Result(agent_id="b", success=True, output="ok"),
        ]
        agg = coord.fan_in(results)
        assert agg.success_rate == 1.0
        assert agg.failed == 0

    def test_all_failure(self):
        coord = self._make_coord()
        results = [
            Result(agent_id="a", success=False, output="", error="e1"),
            Result(agent_id="b", success=False, output="", error="e2"),
        ]
        agg = coord.fan_in(results)
        assert agg.success_rate == 0.0
        assert agg.succeeded == 0

    def test_empty_results(self):
        coord = self._make_coord()
        agg = coord.fan_in([])
        assert agg.total == 0
        assert agg.success_rate == 0.0
        assert not agg.has_conflicts

    def test_summary_contains_agent_output(self):
        coord = self._make_coord()
        results = [
            Result(agent_id="code", success=True, output="Fixed bug in auth.py"),
            Result(agent_id="test", success=False, output="", error="timeout"),
        ]
        agg = coord.fan_in(results)
        summary = agg.summary
        assert "code" in summary
        assert "Fixed bug" in summary
        assert "timeout" in summary

    def test_conflicts_detected_in_fan_in(self):
        coord = self._make_coord()
        results = [
            Result(agent_id="code", success=True, output="Refactor auth.py"),
            Result(agent_id="security", success=True, output="Rewrite auth.py"),
        ]
        agg = coord.fan_in(results)
        assert agg.has_conflicts
        assert "⚠️" in agg.summary

    def test_no_truncation(self):
        """Output is NOT truncated (old fan_in truncated at 200 chars)."""
        coord = self._make_coord()
        long_output = "x" * 500
        results = [Result(agent_id="a", success=True, output=long_output)]
        agg = coord.fan_in(results)
        assert long_output in agg.summary
