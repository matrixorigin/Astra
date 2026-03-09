"""Unit tests for core/models/ab_test.py, core/routing/query_router.py,
cli/profile_manager.py, and core/evaluation/gate_cli.py (format functions)."""

import json
import tempfile
from pathlib import Path

import pytest


# ── ABTestRouter ──────────────────────────────────────────────────────────────

class TestABTestRouter:
    @pytest.fixture
    def router(self):
        from core.models.ab_test import ABTestRouter, ABTestConfig
        r = ABTestRouter()
        r.register(ABTestConfig(
            experiment_name="exp1",
            control_artifact_id="model-v1",
            treatment_artifact_id="model-v2",
            treatment_pct=50,
        ))
        return r

    def test_route_returns_control_or_treatment(self, router):
        from core.models.ab_test import ABTestResult
        result = router.route("exp1", "session-abc")
        assert isinstance(result, ABTestResult)
        assert result.group in ("control", "treatment")
        assert result.artifact_id in ("model-v1", "model-v2")

    def test_route_deterministic(self, router):
        r1 = router.route("exp1", "session-xyz")
        r2 = router.route("exp1", "session-xyz")
        assert r1.group == r2.group

    def test_route_unknown_experiment(self, router):
        assert router.route("nope", "session-1") is None

    def test_treatment_pct_clamped(self):
        from core.models.ab_test import ABTestConfig
        c = ABTestConfig("e", "ctrl", "treat", treatment_pct=150)
        assert c.treatment_pct == 100
        c2 = ABTestConfig("e", "ctrl", "treat", treatment_pct=-10)
        assert c2.treatment_pct == 0

    def test_100pct_always_treatment(self):
        from core.models.ab_test import ABTestRouter, ABTestConfig
        r = ABTestRouter()
        r.register(ABTestConfig("e", "ctrl", "treat", treatment_pct=100))
        for sid in ["s1", "s2", "s3", "s4", "s5"]:
            assert r.route("e", sid).group == "treatment"

    def test_0pct_always_control(self):
        from core.models.ab_test import ABTestRouter, ABTestConfig
        r = ABTestRouter()
        r.register(ABTestConfig("e", "ctrl", "treat", treatment_pct=0))
        for sid in ["s1", "s2", "s3", "s4", "s5"]:
            assert r.route("e", sid).group == "control"

    def test_remove(self, router):
        assert router.remove("exp1") is True
        assert router.route("exp1", "s") is None

    def test_remove_nonexistent(self, router):
        assert router.remove("nope") is False

    def test_list_experiments(self, router):
        assert "exp1" in router.list_experiments()


# ── QueryRouter ───────────────────────────────────────────────────────────────

class TestQueryRouter:
    @pytest.fixture
    def router(self):
        from core.routing.query_router import QueryRouter
        return QueryRouter()

    def test_empty_query_returns_general(self, router):
        from core.routing.query_router import AgentType
        result = router.route("")
        assert result.agent_type == AgentType.GENERAL

    def test_code_query(self, router):
        from core.routing.query_router import AgentType
        result = router.route("Write a Python function to parse JSON")
        assert result.agent_type == AgentType.CODE
        assert result.confidence > 0.3

    def test_debugging_query(self, router):
        from core.routing.query_router import AgentType
        result = router.route("Fix this error: AttributeError in my code")
        assert result.agent_type == AgentType.DEBUGGING

    def test_planning_query(self, router):
        from core.routing.query_router import AgentType
        result = router.route("Design the architecture and plan for a new system")
        assert result.agent_type == AgentType.PLANNING

    def test_general_fallback(self, router):
        from core.routing.query_router import AgentType
        result = router.route("What is the weather today?")
        assert result.agent_type == AgentType.GENERAL

    def test_matched_patterns_populated(self, router):
        result = router.route("Write a Python function")
        assert len(result.matched_patterns) > 0


# ── ProfileManager ────────────────────────────────────────────────────────────

class TestProfileManager:
    @pytest.fixture
    def mgr(self, tmp_path):
        from cli.profile_manager import ProfileManager
        creds = tmp_path / "credentials.json"
        return ProfileManager(credentials_path=creds)

    @pytest.fixture
    def mgr_with_profiles(self, tmp_path):
        from cli.profile_manager import ProfileManager
        creds = tmp_path / "credentials.json"
        data = {
            "current_profile": "alice",
            "profiles": {
                "alice": {"username": "alice", "token": "tok1"},
                "bob": {"username": "bob", "token": "tok2"},
            }
        }
        creds.write_text(json.dumps(data))
        return ProfileManager(credentials_path=creds)

    def test_list_profiles_empty(self, mgr):
        assert mgr.list_profiles() == []

    def test_list_profiles(self, mgr_with_profiles):
        profiles = mgr_with_profiles.list_profiles()
        names = [p["name"] for p in profiles]
        assert "alice" in names
        assert "bob" in names
        current = [p for p in profiles if p["current"]]
        assert current[0]["name"] == "alice"

    def test_get_current_profile(self, mgr_with_profiles):
        assert mgr_with_profiles.get_current_profile() == "alice"

    def test_get_current_profile_missing_file(self, mgr):
        assert mgr.get_current_profile() == "default"

    def test_set_current_profile(self, mgr_with_profiles):
        mgr_with_profiles.set_current_profile("bob")
        assert mgr_with_profiles.get_current_profile() == "bob"

    def test_set_current_profile_not_found(self, mgr_with_profiles):
        with pytest.raises(ValueError, match="not found"):
            mgr_with_profiles.set_current_profile("charlie")

    def test_delete_profile(self, mgr_with_profiles):
        mgr_with_profiles.delete_profile("bob")
        names = [p["name"] for p in mgr_with_profiles.list_profiles()]
        assert "bob" not in names

    def test_delete_current_profile_switches(self, mgr_with_profiles):
        mgr_with_profiles.delete_profile("alice")
        current = mgr_with_profiles.get_current_profile()
        assert current == "bob"

    def test_delete_profile_not_found(self, mgr_with_profiles):
        with pytest.raises(ValueError, match="not found"):
            mgr_with_profiles.delete_profile("nobody")

    def test_load_data_corrupt_file(self, tmp_path):
        from cli.profile_manager import ProfileManager
        creds = tmp_path / "credentials.json"
        creds.write_text("not json{{")
        mgr = ProfileManager(credentials_path=creds)
        assert mgr.get_current_profile() == "default"


# ── gate_cli format functions ─────────────────────────────────────────────────

class TestGateCLIFormat:
    def _pass_result(self):
        return {
            "gate_id": "g1", "verdict": "pass", "reason": "ok",
            "change_type": "prompt", "change_id": "c1",
            "sessions_tested": 10, "created_at": "2026-01-01",
            "snapshot_id": "snap1",
            "metrics": {"error_rate": 0.01, "score_delta": 0.05,
                        "avg_original_score": 0.8, "avg_replay_score": 0.85,
                        "failed_sessions": 0, "total_sessions": 10},
        }

    def _fail_result(self):
        r = self._pass_result()
        r["verdict"] = "fail"
        r["reason"] = "regression"
        return r

    def _skip_result(self):
        return {
            "gate_id": "g2", "verdict": "skip", "reason": "no sessions",
            "change_type": "skill", "change_id": "c2", "created_at": "2026-01-01",
        }

    def test_format_github_comment_pass(self):
        from core.evaluation.gate_cli import format_github_comment
        comment = format_github_comment(self._pass_result())
        assert "✅" in comment
        assert "PASS" in comment
        assert "prompt" in comment

    def test_format_github_comment_fail(self):
        from core.evaluation.gate_cli import format_github_comment
        comment = format_github_comment(self._fail_result())
        assert "❌" in comment
        assert "FAIL" in comment
        assert "Action Required" in comment

    def test_format_github_comment_skip(self):
        from core.evaluation.gate_cli import format_github_comment
        comment = format_github_comment(self._skip_result())
        assert "SKIPPED" in comment
        assert "no sessions" in comment

    def test_format_json(self):
        from core.evaluation.gate_cli import format_json
        result = self._pass_result()
        output = format_json(result)
        parsed = json.loads(output)
        assert parsed["verdict"] == "pass"
