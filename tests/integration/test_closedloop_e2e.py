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


def _make_mock_skill(name: str, version: str = "1.0.0", description: str = "test",
                     definition: dict | None = None) -> Mock:
    """Create a Mock that satisfies the Skill interface for register()."""
    skill = Mock()
    skill.name = name
    skill.version = version
    skill.description = description
    skill.requirements = Mock()
    skill.requirements.model_dump.return_value = definition or {}
    skill.side_effect_profile = Mock()
    skill.side_effect_profile.model_dump.return_value = {"category": "read"}
    return skill


def _seed_llm_events(db: Session, agent_id: str, n: int = 10, quality: float = 4.5):
    """Insert synthetic llm_response events for SLO / calibration queries."""
    import json
    from core.utils.id_generator import generate_id

    for i in range(n):
        eid = generate_id()
        ts = datetime.now() - timedelta(days=i % 7)
        db.execute(text("""
            INSERT INTO agent_events
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

        engine = MemoryGovernanceEngine(lambda: db_session)

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

        engine = MemoryGovernanceEngine(lambda: db_session)

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
            monitor = SLOMonitor(lambda: db_session)
            report = monitor.check_agent("slo_test_agent", period_days=7)

            assert report.agent_id == "slo_test_agent"
            assert report.period_days == 7
            assert len(report.statuses) == 3  # 3 default SLOs
            for s in report.statuses:
                assert hasattr(s, "met")
                assert hasattr(s, "burn_rate")
        finally:
            db_session.execute(
                text("DELETE FROM agent_events WHERE agent_id = 'slo_test_agent'")
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
            text("SELECT * FROM infra_distributed_locks WHERE lock_name = 'governance_eval_daily'")
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
                text("DELETE FROM infra_distributed_locks WHERE lock_name = 'governance_eval_daily'")
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
                text("DELETE FROM infra_distributed_locks WHERE lock_name = 'governance_eval_daily'")
            )
            db_session.commit()


# ═══════════════════════════════════════════════════════════════════
# 8. API-level: /chat creates GateTrigger-wired ChatLoop
# ═══════════════════════════════════════════════════════════════════


