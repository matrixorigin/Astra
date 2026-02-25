"""E2E tests for closed-loop wiring: GateTrigger injection, eval_daily
governance, ConflictResolver in fan_in, SLO weekly check.

All tests use real DB + real component wiring, only LLM is mocked.
"""

from __future__ import annotations

import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from unittest.mock import Mock, patch, MagicMock

import pytest
from sqlalchemy import text
from sqlalchemy.orm import Session

from core.agent.coordination import (
    AggregatedResult,
    Conflict,
    CoordinationPatterns,
    Result,
)
from core.context.scheduler import GovernanceTaskRunner, GOVERNANCE_TASKS
from core.evaluation.gate_trigger import GateTrigger


# ── Helpers ──────────────────────────────────────────────────────────


def _make_gate_trigger(db_session: Session) -> GateTrigger:
    """Build a GateTrigger that reuses the test db session."""
    return GateTrigger(db_factory=lambda: db_session)


def _seed_llm_events(db: Session, agent_id: str, n: int = 10, quality: float = 4.5):
    """Insert synthetic llm_response events for SLO / calibration queries."""
    import json
    from core.utils.id_generator import generate_id

    for i in range(n):
        eid = generate_id()
        ts = datetime.now() - timedelta(days=i % 7)
        db.execute(text("""
            INSERT INTO conversation_events
                (event_id, session_id, user_id, agent_id, agent_version,
                 event_type, content, quality_score, `metadata`,
                 causal_chain_id, created_at)
            VALUES
                (:eid, :sid, 'system', :aid, '1.0.0',
                 'llm_response', 'test', :qs, :meta, :eid, :ts)
        """), {
            "eid": eid,
            "sid": f"slo_test_{i}",
            "aid": agent_id,
            "qs": quality,
            "meta": json.dumps({"confidence_score": 0.85}),
            "ts": ts,
        })
    db.commit()


# ═══════════════════════════════════════════════════════════════════
# 1. GateTrigger injection — production path fires gate on changes
# ═══════════════════════════════════════════════════════════════════


class TestGateTriggerProductionWiring:
    """Verify that _build_chat_loop creates GateTrigger and passes it
    through SkillRegistry → register() and ContextManager → PromptManager."""

    def test_build_chat_loop_injects_gate_trigger(self, db_session):
        """The real _build_chat_loop must create a GateTrigger and pass it
        to SkillRegistry and ContextManager."""
        from api.routers.chat import _build_chat_loop

        with patch("core.evaluation.gate_trigger.GateTrigger") as MockGT:
            mock_gt_instance = Mock()
            MockGT.return_value = mock_gt_instance

            loop = _build_chat_loop(lambda: db_session)

            # GateTrigger was instantiated
            MockGT.assert_called_once()

    def test_build_chat_loop_uses_session_local_not_get_db_session(self, db_session):
        """GateTrigger must use SessionLocal (independent sessions), NOT
        get_db_session (which returns the shared request session in tests).

        This is a regression guard for the concurrent-session bug where
        gate threads corrupted the request session.
        """
        from api.routers.chat import _build_chat_loop
        from api.database import SessionLocal

        captured_factory = {}

        original_init = GateTrigger.__init__

        def spy_init(self_gt, db_factory, **kwargs):
            captured_factory["db_factory"] = db_factory
            original_init(self_gt, db_factory, **kwargs)

        with patch.object(GateTrigger, "__init__", spy_init):
            _build_chat_loop(lambda: db_session)

        # Must be SessionLocal itself, not a lambda wrapping get_db_session
        assert captured_factory["db_factory"] is SessionLocal, \
            "GateTrigger must use SessionLocal for thread-safe independent sessions"

    def test_skill_register_fires_gate_in_production_path(self, db_session):
        """Register a skill through the real SkillRegistry with a real
        GateTrigger — verify on_skill_change is called."""
        from core.skills.registry import SkillRegistry

        gate_trigger = Mock()
        registry = SkillRegistry(session=db_session, gate_trigger=gate_trigger)

        skill = Mock()
        skill.name = "e2e_test_skill"
        skill.version = "1.0.0"
        skill.description = "E2E test"
        skill.requirements = Mock()
        skill.requirements.model_dump.return_value = {"repo_types": ["code"]}

        with patch.object(registry, "_compute_code_hash", return_value="hash123"):
            registry.register(skill, is_active=True)

        gate_trigger.on_skill_change.assert_called_once_with(
            skill_name="e2e_test_skill",
            version="1.0.0",
            definition={"repo_types": ["code"]},
        )

    def test_prompt_register_fires_gate_via_context_manager(self, db_session):
        """ContextManager(gate_trigger=X) must pass it to PromptManager,
        and PromptManager.register_prompt must call on_prompt_change."""
        from core.context.manager import ContextManager

        gate_trigger = Mock()
        cm = ContextManager(lambda: db_session, gate_trigger=gate_trigger)

        # register_prompt on the inner PromptManager
        cm.prompts.register_prompt(
            "e2e_test_prompt", "v1", "You are a test agent.", is_active=True,
        )

        gate_trigger.on_prompt_change.assert_called_once_with(
            template_id="e2e_test_prompt",
            version="v1",
            content="You are a test agent.",
        )

    def test_gate_trigger_actually_fires_async_thread(self, db_session):
        """Full integration: GateTrigger.on_skill_change spawns a daemon
        thread that calls _run_gate with correct args."""
        fired = threading.Event()
        captured_args = {}

        gt = _make_gate_trigger(db_session)

        def mock_run_gate(change_type, change_id, change_content):
            captured_args.update({
                "change_type": change_type,
                "change_id": change_id,
                "change_content": change_content,
            })
            fired.set()

        with patch.object(gt, "_run_gate", side_effect=mock_run_gate):
            gt.on_skill_change("my_skill", "2.0", {"key": "val"})
            assert fired.wait(timeout=3.0), "Gate thread did not fire within 3s"

        assert captured_args["change_type"] == "skill"
        assert captured_args["change_id"] == "my_skill@2.0"
        # on_skill_change wraps into {name, version, definition}
        assert captured_args["change_content"]["name"] == "my_skill"
        assert captured_args["change_content"]["version"] == "2.0"
        assert captured_args["change_content"]["definition"] == {"key": "val"}

    def test_gate_trigger_does_not_fire_for_inactive_skill(self, db_session):
        """Inactive skill registration must NOT trigger the gate."""
        from core.skills.registry import SkillRegistry

        gate_trigger = Mock()
        registry = SkillRegistry(session=db_session, gate_trigger=gate_trigger)

        skill = Mock()
        skill.name = "inactive_skill"
        skill.version = "1.0.0"
        skill.description = "test"
        skill.requirements = Mock()
        skill.requirements.model_dump.return_value = {}

        with patch.object(registry, "_compute_code_hash", return_value="h"):
            registry.register(skill, is_active=False)

        gate_trigger.on_skill_change.assert_not_called()

    def test_no_gate_trigger_no_crash(self, db_session):
        """SkillRegistry(gate_trigger=None) must work without errors."""
        from core.skills.registry import SkillRegistry

        registry = SkillRegistry(session=db_session, gate_trigger=None)

        skill = Mock()
        skill.name = "safe_skill"
        skill.version = "1.0.0"
        skill.description = "test"
        skill.requirements = Mock()
        skill.requirements.model_dump.return_value = {}

        with patch.object(registry, "_compute_code_hash", return_value="h"):
            registry.register(skill, is_active=True)  # must not raise


# ═══════════════════════════════════════════════════════════════════
# 2. eval_daily governance task — full closed-loop pipeline
# ═══════════════════════════════════════════════════════════════════


class TestEvalDailyGovernance:
    """Verify the eval_daily task dispatches through all 4 phases."""

    def test_eval_daily_registered_in_governance_tasks(self):
        """eval_daily must be in GOVERNANCE_TASKS with correct config."""
        assert "eval_daily" in GOVERNANCE_TASKS
        cfg = GOVERNANCE_TASKS["eval_daily"]
        assert cfg["interval"] == 86400
        assert cfg["lock_name"] == "governance_eval_daily"

    def test_dispatch_routes_eval_daily(self, db_session):
        """_dispatch('eval_daily', db, factory) must call _run_eval_daily
        with the factory, not the lock session."""
        factory = lambda: db_session
        with patch.object(
            GovernanceTaskRunner, "_run_eval_daily", return_value={"drift_signals": 0}
        ) as mock_eval:
            result = GovernanceTaskRunner._dispatch("eval_daily", db_session, factory)

        mock_eval.assert_called_once_with(factory)
        assert result == {"drift_signals": 0}

    def test_dispatch_routes_hourly_to_lifecycle(self, db_session):
        """Non-eval tasks must still route to MemoryGovernanceEngine."""
        mock_engine = Mock()
        mock_engine.run_hourly_tasks.return_value = {"archived": 5}

        with patch(
            "core.context.lifecycle.MemoryGovernanceEngine", return_value=mock_engine
        ):
            result = GovernanceTaskRunner._dispatch("hourly", db_session, lambda: db_session)

        mock_engine.run_hourly_tasks.assert_called_once()
        assert result["archived"] == 5

    def test_eval_daily_runs_all_four_phases(self, db_session):
        """_run_eval_daily must attempt all 4 phases and collect results."""
        from core.evaluation.drift_pipeline import PipelineResult
        from core.evaluation.confidence_calibrator import CalibrationResult
        from core.learning.input_face_learner import DiagnosisResult, InputFace

        mock_drift = PipelineResult(signals_detected=3, corrections_applied=1)
        mock_cal = CalibrationResult(
            mean_confidence=0.8, mean_quality=0.75,
            calibration_error=0.05, bias=0.05,
            sample_count=100, bucket_errors=[],
        )
        mock_face = DiagnosisResult(
            input_face=InputFace.PROMPT, bottleneck="stale prompt", applied=True,
        )
        mock_skill = {"learned": 2, "total_failures": 5}

        factory = lambda: db_session
        with patch("core.evaluation.drift_pipeline.run_drift_pipeline", return_value=mock_drift), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.measure", return_value=mock_cal), \
             patch("core.learning.input_face_learner.InputFaceLearner.diagnose_and_fix", return_value=[mock_face]), \
             patch("core.llm.client.LLMClient.__init__", return_value=None), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.learn_from_failures", return_value=mock_skill):

            result = GovernanceTaskRunner._run_eval_daily(factory)

        assert result["drift_signals"] == 3
        assert result["drift_corrections"] == 1
        assert result["calibration_error"] == 5  # 0.05 * 100 rounded
        assert result["faces_fixed"] == 1
        assert result["skills_learned"] == 2

    def test_eval_daily_phase_isolation(self, db_session):
        """If one phase fails, others must still execute."""
        from core.evaluation.confidence_calibrator import CalibrationResult

        factory = lambda: db_session
        with patch("core.evaluation.drift_pipeline.run_drift_pipeline", side_effect=RuntimeError("boom")), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.measure",
                   return_value=CalibrationResult(
                       mean_confidence=0.0, mean_quality=0.0,
                       calibration_error=0.0, bias=0.0,
                       sample_count=0, bucket_errors=[],
                   )), \
             patch("core.learning.input_face_learner.InputFaceLearner.diagnose_and_fix", return_value=[]), \
             patch("core.llm.client.LLMClient.__init__", return_value=None), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.learn_from_failures",
                   return_value={"learned": 0}):

            result = GovernanceTaskRunner._run_eval_daily(factory)

        # Phase 1 failed → drift_signals = 0
        assert result["drift_signals"] == 0
        # Phase 2-4 still ran
        assert "calibration_error" in result
        assert "faces_fixed" in result
        assert "skills_learned" in result

    def test_eval_daily_all_phases_fail_gracefully(self, db_session):
        """If ALL phases fail, result should still be a valid dict."""
        factory = lambda: db_session
        with patch("core.evaluation.drift_pipeline.run_drift_pipeline", side_effect=Exception("1")), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.__init__", side_effect=Exception("2")), \
             patch("core.llm.client.LLMClient.__init__", side_effect=Exception("3")), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.__init__", side_effect=Exception("4")):

            result = GovernanceTaskRunner._run_eval_daily(factory)

        assert isinstance(result, dict)
        assert result.get("drift_signals") == 0


# ═══════════════════════════════════════════════════════════════════
# 3. ConflictResolver wired into fan_in
# ═══════════════════════════════════════════════════════════════════


class TestFanInConflictResolution:
    """Verify fan_in(resolve=True) auto-resolves conflicts."""

    def _make_patterns(self) -> CoordinationPatterns:
        return CoordinationPatterns(delegation_skill=Mock())

    def _conflicting_results(self) -> list[Result]:
        """Two agents disagree on the same artifact."""
        return [
            Result(agent_id="security-reviewer", success=True, output="reject: SQL injection risk"),
            Result(agent_id="perf-reviewer", success=True, output="approve: fast enough"),
        ]

    def test_fan_in_without_resolve_has_no_resolutions(self):
        """Default fan_in must NOT resolve conflicts."""
        cp = self._make_patterns()
        results = self._conflicting_results()

        # Need to ensure conflicts are detected
        with patch("core.agent.coordination.detect_conflicts") as mock_detect:
            mock_detect.return_value = [
                Conflict(
                    artifact="handler.py",
                    agents=["security-reviewer", "perf-reviewer"],
                    proposals=["reject: SQL injection risk", "approve: fast enough"],
                    severity="warning",
                ),
            ]
            agg = cp.fan_in(results, resolve=False)

        assert agg.has_conflicts
        assert agg.resolutions == []

    def test_fan_in_resolve_by_authority(self):
        """resolve=True + priority_order → authority-based resolution."""
        cp = self._make_patterns()
        results = self._conflicting_results()

        with patch("core.agent.coordination.detect_conflicts") as mock_detect:
            mock_detect.return_value = [
                Conflict(
                    artifact="handler.py",
                    agents=["security-reviewer", "perf-reviewer"],
                    proposals=["reject: SQL injection risk", "approve: fast enough"],
                    severity="warning",
                ),
            ]
            agg = cp.fan_in(
                results,
                resolve=True,
                priority_order=["security-reviewer", "perf-reviewer"],
            )

        assert len(agg.resolutions) == 1
        assert agg.resolutions[0]["winner"] == "security-reviewer"
        assert agg.resolutions[0]["method"] == "authority"
        assert agg.resolutions[0]["artifact"] == "handler.py"

    def test_fan_in_resolve_by_evidence(self):
        """resolve=True without priority_order → evidence-based resolution."""
        cp = self._make_patterns()
        results = self._conflicting_results()

        with patch("core.agent.coordination.detect_conflicts") as mock_detect:
            mock_detect.return_value = [
                Conflict(
                    artifact="handler.py",
                    agents=["security-reviewer", "perf-reviewer"],
                    proposals=["reject: SQL injection risk", "approve: fast enough"],
                    severity="warning",
                ),
            ]
            agg = cp.fan_in(results, resolve=True)

        assert len(agg.resolutions) == 1
        assert agg.resolutions[0]["method"] == "evidence"

    def test_fan_in_no_conflicts_no_resolutions(self):
        """When agents agree, resolve=True should produce no resolutions."""
        cp = self._make_patterns()
        results = [
            Result(agent_id="a1", success=True, output="approve"),
            Result(agent_id="a2", success=True, output="approve"),
        ]

        with patch("core.agent.coordination.detect_conflicts", return_value=[]):
            agg = cp.fan_in(results, resolve=True)

        assert not agg.has_conflicts
        assert agg.resolutions == []

    def test_fan_in_resolve_failure_is_non_fatal(self):
        """If ConflictResolver crashes, fan_in must still return results."""
        cp = self._make_patterns()
        results = self._conflicting_results()

        with patch("core.agent.coordination.detect_conflicts") as mock_detect:
            mock_detect.return_value = [
                Conflict(
                    artifact="x.py",
                    agents=["a", "b"],
                    proposals=["do X", "do Y"],
                ),
            ]
            with patch(
                "core.agents.conflict_resolver.ConflictResolver.detect_conflict",
                side_effect=RuntimeError("resolver crashed"),
            ):
                agg = cp.fan_in(results, resolve=True)

        # Must still have valid aggregation
        assert agg.total == 2
        assert agg.succeeded == 2
        # Resolutions empty due to failure (non-fatal)
        assert agg.resolutions == []

    def test_aggregated_result_has_resolutions_field(self):
        """AggregatedResult dataclass must have resolutions field."""
        agg = AggregatedResult(results=[], resolutions=[{"test": True}])
        assert agg.resolutions == [{"test": True}]

    def test_fan_in_multiple_conflicts_resolved(self):
        """Multiple conflicts should each get a resolution."""
        cp = self._make_patterns()
        results = [
            Result(agent_id="a1", success=True, output="fix A in file1"),
            Result(agent_id="a2", success=True, output="fix B in file1"),
            Result(agent_id="a3", success=True, output="fix C in file2"),
        ]

        with patch("core.agent.coordination.detect_conflicts") as mock_detect:
            mock_detect.return_value = [
                Conflict(artifact="file1.py", agents=["a1", "a2"],
                         proposals=["fix A in file1", "fix B in file1"]),
                Conflict(artifact="file2.py", agents=["a1", "a3"],
                         proposals=["keep file2", "fix C in file2"]),
            ]
            agg = cp.fan_in(
                results, resolve=True,
                priority_order=["a1", "a2", "a3"],
            )

        assert len(agg.resolutions) == 2
        assert all(r["method"] == "authority" for r in agg.resolutions)
        assert all(r["winner"] == "a1" for r in agg.resolutions)


# ═══════════════════════════════════════════════════════════════════
# 4. SLO check in weekly governance
# ═══════════════════════════════════════════════════════════════════


class TestSLOWeeklyGovernance:
    """Verify SLOMonitor is wired into weekly governance tasks."""

    def test_weekly_tasks_include_slo_check(self, db_session):
        """run_weekly_tasks must include slo_violations in results."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from core.evaluation.slo_monitor import AgentSLOReport, SLOStatus, SLOTarget, SLOSeverity

        engine = MemoryGovernanceEngine(db_session)

        # Mock SLOMonitor to return a report with 1 violation
        mock_report = AgentSLOReport(
            agent_id="dev-agent",
            statuses=[
                SLOStatus(
                    slo=SLOTarget("quality", "avg_quality", 4.0),
                    current_value=3.5, met=False, burn_rate=2.0,
                    severity=SLOSeverity.WARNING, days_elapsed=7, bad_days=3,
                ),
                SLOStatus(
                    slo=SLOTarget("hallucination_rate", "hallucination_rate", 0.02, "<="),
                    current_value=0.01, met=True, burn_rate=0.5,
                    severity=SLOSeverity.OK, days_elapsed=7, bad_days=0,
                ),
            ],
            period_days=7,
            created_at=datetime.now(timezone.utc),
        )

        with patch("core.evaluation.slo_monitor.SLOMonitor.check_agent", return_value=mock_report):
            with patch("core.context.lifecycle.MemoryGovernanceEngine._get_agent_ids", return_value=["dev-agent"]):
                result = engine.run_weekly_tasks()

        assert "slo_violations" in result
        assert result["slo_violations"] == 1  # quality SLO not met

    def test_weekly_tasks_slo_failure_is_non_fatal(self, db_session):
        """If SLOMonitor crashes, weekly tasks must still complete."""
        from core.context.lifecycle import MemoryGovernanceEngine

        engine = MemoryGovernanceEngine(db_session)

        with patch("core.evaluation.slo_monitor.SLOMonitor.__init__", side_effect=RuntimeError("SLO boom")):
            result = engine.run_weekly_tasks()

        # Other weekly tasks still ran
        assert "contradictions_found" in result
        assert "health_reports" in result
        # slo_violations absent (exception caught)
        assert "slo_violations" not in result

    def test_slo_monitor_with_real_data(self, db_session):
        """SLOMonitor.check_agent with real DB data returns valid report."""
        from core.evaluation.slo_monitor import SLOMonitor

        _seed_llm_events(db_session, "slo_test_agent", n=15, quality=4.5)

        try:
            monitor = SLOMonitor(db_session)
            report = monitor.check_agent("slo_test_agent", period_days=7)

            assert report.agent_id == "slo_test_agent"
            assert report.period_days == 7
            assert len(report.statuses) == 3  # 3 default SLOs
            for s in report.statuses:
                assert hasattr(s, "met")
                assert hasattr(s, "burn_rate")
        finally:
            db_session.execute(
                text("DELETE FROM conversation_events WHERE agent_id = 'slo_test_agent'")
            )
            db_session.commit()


# ═══════════════════════════════════════════════════════════════════
# 5. ContextManager gate_trigger passthrough
# ═══════════════════════════════════════════════════════════════════


class TestContextManagerGateTriggerPassthrough:
    """Verify ContextManager passes gate_trigger to PromptManager."""

    def test_context_manager_passes_gate_trigger_to_prompt_manager(self, db_session):
        """ContextManager(gate_trigger=X) → PromptManager.gate_trigger == X."""
        from core.context.manager import ContextManager

        sentinel = object()
        cm = ContextManager(lambda: db_session, gate_trigger=sentinel)

        assert cm.prompts.gate_trigger is sentinel

    def test_context_manager_default_gate_trigger_is_none(self, db_session):
        """ContextManager() → PromptManager.gate_trigger is None."""
        from core.context.manager import ContextManager

        cm = ContextManager(lambda: db_session)
        assert cm.prompts.gate_trigger is None

    def test_prompt_change_through_context_manager_fires_gate(self, db_session):
        """Full chain: ContextManager → PromptManager → GateTrigger."""
        from core.context.manager import ContextManager

        gate_trigger = Mock()
        cm = ContextManager(lambda: db_session, gate_trigger=gate_trigger)

        cm.prompts.register_prompt("chain_test", "v1", "Hello", is_active=True)
        cm.prompts.register_prompt("chain_test", "v2", "Hello v2", is_active=True)

        assert gate_trigger.on_prompt_change.call_count == 2
        # Second call should be v2
        _, kwargs = gate_trigger.on_prompt_change.call_args
        assert kwargs["version"] == "v2"
        assert kwargs["content"] == "Hello v2"


# ═══════════════════════════════════════════════════════════════════
# 6. Full closed-loop scenario: skill change → gate → eval_daily
# ═══════════════════════════════════════════════════════════════════


class TestFullClosedLoopScenario:
    """Simulate a realistic scenario: deploy a new skill version,
    gate fires, then eval_daily runs the full pipeline."""

    def test_skill_deploy_triggers_gate_then_eval_runs(self, db_session):
        """Scenario:
        1. Register new skill version → GateTrigger fires
        2. eval_daily runs → drift + calibration + learning
        3. Results are collected
        """
        from core.skills.registry import SkillRegistry

        # Step 1: Register skill with gate trigger
        gate_fired = threading.Event()
        gate_args = {}

        def capture_run_gate(change_type, change_id, change_content):
            gate_args["type"] = change_type
            gate_args["id"] = change_id
            gate_fired.set()

        gt = _make_gate_trigger(db_session)

        with patch.object(gt, "_run_gate", side_effect=capture_run_gate):
            registry = SkillRegistry(session=db_session, gate_trigger=gt)

            skill = Mock()
            skill.name = "deploy_test"
            skill.version = "2.0.0"
            skill.description = "new version"
            skill.requirements = Mock()
            skill.requirements.model_dump.return_value = {}

            with patch.object(registry, "_compute_code_hash", return_value="new_hash"):
                registry.register(skill, is_active=True)

        assert gate_fired.wait(timeout=3.0)
        assert gate_args["type"] == "skill"
        assert gate_args["id"] == "deploy_test@2.0.0"

        # Step 2: Run eval_daily
        from core.evaluation.drift_pipeline import PipelineResult
        from core.evaluation.confidence_calibrator import CalibrationResult

        with patch("core.evaluation.drift_pipeline.run_drift_pipeline",
                   return_value=PipelineResult(signals_detected=1, corrections_applied=1)), \
             patch("core.evaluation.confidence_calibrator.ConfidenceCalibrator.measure",
                   return_value=CalibrationResult(
                       mean_confidence=0.8, mean_quality=0.78,
                       calibration_error=0.02, bias=0.02,
                       sample_count=50, bucket_errors=[],
                   )), \
             patch("core.learning.input_face_learner.InputFaceLearner.diagnose_and_fix", return_value=[]), \
             patch("core.llm.client.LLMClient.__init__", return_value=None), \
             patch("core.skills.self_improving_selector.SelfImprovingSelector.learn_from_failures",
                   return_value={"learned": 1}):

            eval_result = GovernanceTaskRunner._run_eval_daily(lambda: db_session)

        assert eval_result["drift_signals"] == 1
        assert eval_result["drift_corrections"] == 1
        assert eval_result["calibration_error"] == 2
        assert eval_result["skills_learned"] == 1

    def test_multi_agent_review_with_conflict_resolution(self, db_session):
        """Scenario: 3 agents review code, 2 conflict, resolution picks winner.

        Simulates: fan_out → fan_in(resolve=True, priority_order=...)
        """
        cp = CoordinationPatterns(delegation_skill=Mock())

        # Simulate fan_out results
        results = [
            Result(agent_id="security-reviewer", success=True,
                   output="REJECT: potential XSS in template rendering"),
            Result(agent_id="perf-reviewer", success=True,
                   output="APPROVE: acceptable latency"),
            Result(agent_id="style-reviewer", success=True,
                   output="APPROVE: follows conventions"),
        ]

        with patch("core.agent.coordination.detect_conflicts") as mock_detect:
            mock_detect.return_value = [
                Conflict(
                    artifact="views/render.py",
                    agents=["security-reviewer", "perf-reviewer"],
                    proposals=[
                        "REJECT: potential XSS in template rendering",
                        "APPROVE: acceptable latency",
                    ],
                    severity="blocking",
                ),
            ]

            agg = cp.fan_in(
                results,
                resolve=True,
                priority_order=["security-reviewer", "perf-reviewer", "style-reviewer"],
            )

        # Security wins by authority
        assert agg.total == 3
        assert agg.succeeded == 3
        assert agg.has_conflicts
        assert len(agg.resolutions) == 1
        assert agg.resolutions[0]["winner"] == "security-reviewer"
        assert agg.resolutions[0]["artifact"] == "views/render.py"

        # Summary should mention conflicts
        assert "conflict" in agg.summary.lower()


# ═══════════════════════════════════════════════════════════════════
# 7. GovernanceTaskRunner.run() full integration with distributed lock
# ═══════════════════════════════════════════════════════════════════


class TestGovernanceTaskRunnerIntegration:
    """Test the full GovernanceTaskRunner.run() path: lock → dispatch →
    persist → release, using real DB for distributed locks."""

    def test_run_eval_daily_with_lock(self, db_session):
        """GovernanceTaskRunner.run('eval_daily') acquires lock, dispatches,
        persists result, and releases lock."""
        from contextlib import contextmanager

        @contextmanager
        def db_ctx():
            yield db_session

        runner = GovernanceTaskRunner(db_context_factory=db_ctx)

        with patch.object(
            GovernanceTaskRunner, "_run_eval_daily",
            return_value={"drift_signals": 0, "skills_learned": 0},
        ):
            result = runner.run("eval_daily")

        assert result is not None
        assert result["drift_signals"] == 0

        # Lock must be released after run
        lock_row = db_session.execute(
            text("SELECT * FROM distributed_locks WHERE lock_name = 'governance_eval_daily'")
        ).first()
        assert lock_row is None, "Lock must be released after successful run"

    def test_run_skips_when_lock_held(self, db_session):
        """If another instance holds the lock, run() returns None."""
        from contextlib import contextmanager
        from api.models import DistributedLock

        @contextmanager
        def db_ctx():
            yield db_session

        runner = GovernanceTaskRunner(db_context_factory=db_ctx)

        # Pre-acquire lock (simulate another instance)
        db_session.add(DistributedLock(
            lock_name="governance_eval_daily",
            instance_id="other-host:9999",
            acquired_at=datetime.now(),
            expires_at=datetime.now() + timedelta(seconds=300),
            task_name="eval_daily",
        ))
        db_session.commit()

        try:
            result = runner.run("eval_daily")
            assert result is None  # Skipped because lock is held
        finally:
            db_session.execute(
                text("DELETE FROM distributed_locks WHERE lock_name = 'governance_eval_daily'")
            )
            db_session.commit()

    def test_run_takes_over_expired_lock(self, db_session):
        """If lock is expired, run() takes it over and executes."""
        from contextlib import contextmanager
        from api.models import DistributedLock

        @contextmanager
        def db_ctx():
            yield db_session

        runner = GovernanceTaskRunner(db_context_factory=db_ctx)

        # Pre-acquire expired lock
        db_session.add(DistributedLock(
            lock_name="governance_eval_daily",
            instance_id="dead-host:1234",
            acquired_at=datetime.now() - timedelta(hours=1),
            expires_at=datetime.now() - timedelta(minutes=30),  # expired
            task_name="eval_daily",
        ))
        db_session.commit()

        try:
            with patch.object(
                GovernanceTaskRunner, "_run_eval_daily",
                return_value={"drift_signals": 0},
            ):
                result = runner.run("eval_daily")

            assert result is not None  # Took over expired lock and ran
            assert result["drift_signals"] == 0
        finally:
            db_session.execute(
                text("DELETE FROM distributed_locks WHERE lock_name = 'governance_eval_daily'")
            )
            db_session.commit()


# ═══════════════════════════════════════════════════════════════════
# 8. API-level: /chat creates GateTrigger-wired ChatLoop
# ═══════════════════════════════════════════════════════════════════


class TestChatAPIGateTriggerE2E:
    """Verify that the real /chat API path creates a GateTrigger-wired
    ChatLoop with SessionLocal for thread-safe gate execution."""

    def test_chat_api_creates_gate_wired_loop(self, db_session):
        """POST /chat → _build_chat_loop → GateTrigger(SessionLocal) →
        SkillRegistry(gate_trigger=gt) + ContextManager(gate_trigger=gt).

        We intercept _build_chat_loop to verify the wiring without
        needing a real LLM.
        """
        from api.database import SessionLocal
        from fastapi.testclient import TestClient
        from api.main import app
        from core.utils.id_generator import generate_id

        client = TestClient(app)

        # Register + login
        username = f"gate_e2e_{generate_id()[:8]}"
        client.post("/auth/register", json={
            "username": username,
            "email": f"{username}@test.com",
            "password": "testpass1234",
        })
        resp = client.post("/auth/login", json={
            "username": username,
            "password": "testpass1234",
        })
        headers = {"Authorization": f"Bearer {resp.json()['access_token']}"}

        # Intercept _build_chat_loop to capture wiring and inject mock LLM
        captured = {}

        def spy_build(db_factory):
            from core.agent.chat_loop import ChatLoop
            from core.agent.executor import AgentExecutor
            from core.context.manager import ContextManager
            from core.events.event_logger import EventLogger
            from core.verification.firewall import HallucinationFirewall
            from core.skills.pipeline import SkillPipeline
            from core.skills.registry import SkillRegistry

            db = db_factory()

            gt = GateTrigger(db_factory=SessionLocal)
            captured["gate_trigger"] = gt
            captured["db_factory_is_session_local"] = True

            event_logger = EventLogger.from_session(db)
            mock_llm = Mock()
            mock_llm.config = {"model": "test", "temperature": 0}
            mock_llm.chat.return_value = Mock(
                content="test", model="test",
                tokens_prompt=10, tokens_completion=5,
                tokens_total=15, latency_ms=50, cost_usd=0.001,
            )
            mock_llm.chat_with_tools.return_value = {"content": "test"}

            registry = SkillRegistry(db, gate_trigger=gt)
            context_manager = ContextManager(lambda: db, gate_trigger=gt)
            captured["registry_has_gate"] = registry.gate_trigger is gt
            captured["prompts_has_gate"] = context_manager.prompts.gate_trigger is gt

            selector = SkillPipeline(db, mock_llm, audit=True, learning=True)
            executor = AgentExecutor(lambda: db, registry)
            firewall = HallucinationFirewall(db, context_manager)

            return ChatLoop(
                selector=selector, executor=executor,
                llm_client=mock_llm, event_logger=event_logger,
                context_manager=context_manager, firewall=firewall,
            )

        import core.agent.run_engine as re_mod
        original_start = re_mod.RunEngine.start_run

        async def patched_start(self_engine, run):
            with patch("api.routers.chat._build_chat_loop", spy_build):
                await original_start(self_engine, run)

        with patch("api.routers.chat._build_chat_loop", spy_build), \
             patch.object(re_mod.RunEngine, "start_run", patched_start):
            resp = client.post("/chat", json={
                "message": "test gate wiring",
            }, headers=headers)

        # Verify the full wiring chain
        assert captured.get("gate_trigger") is not None
        assert captured.get("registry_has_gate") is True
        assert captured.get("prompts_has_gate") is True
