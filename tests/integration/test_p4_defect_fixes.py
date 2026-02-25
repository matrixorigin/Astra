"""P4 Phase C — tests for defect fixes D1-D6.

Covers:
  - D1: Planning path CoT audit blocks malicious tool calls
  - D2: Planning path records HITL outcome
  - D3: json.dumps serialization (drift_detector + regression_gate)
  - D4: DriftCorrector._apply_fallback persists to DB
  - D5: Narrow exception catch in distributed lock acquire
  - D6: SLO checks all agents, not just hardcoded one
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from unittest.mock import Mock, patch, MagicMock

import pytest
from sqlalchemy import text
from sqlalchemy.orm import Session


# ── D1: Planning path CoT audit ──────────────────────────────────


class TestPlanningPathCoTAudit:
    """run_step_with_planning must call audit_tool_call before skill execution."""

    def test_cot_audit_present_in_planning_path(self, db_session):
        """Verify audit_tool_call is invoked for each planning step skill."""
        import inspect
        from core.agent.chat_loop import ChatLoop

        source = inspect.getsource(ChatLoop.run_step_with_planning)
        assert "audit_tool_call" in source, "Planning path must call audit_tool_call"
        # Must import it
        assert "from core.verification.cot_audit import audit_tool_call" in source

    def test_cot_audit_blocks_in_planning_path(self, db_session):
        """If CoT audit returns unsafe, planning step must be blocked."""
        import inspect
        from core.agent.chat_loop import ChatLoop

        source = inspect.getsource(ChatLoop.run_step_with_planning)
        assert "audit.safe" in source
        assert "Blocked by CoT audit" in source


# ── D2: Planning path records HITL outcome ───────────────────────


class TestPlanningPathRecordOutcome:
    """run_step_with_planning must call hitl_policy.record_outcome."""

    def test_record_outcome_in_planning_path(self, db_session):
        """Planning path source must contain record_outcome call."""
        import inspect
        from core.agent.chat_loop import ChatLoop

        source = inspect.getsource(ChatLoop.run_step_with_planning)
        assert "record_outcome" in source, "Planning path must call record_outcome"


# ── D3: json.dumps serialization ─────────────────────────────────


class TestJsonSerialization:
    """str() replaced with json.dumps() in drift_detector and regression_gate."""

    def test_drift_record_uses_json(self, db_session):
        """DriftCorrector._record must produce valid JSON content."""
        from core.evaluation.drift_detector import DriftCorrector, DriftSignal, DriftSeverity

        corrector = DriftCorrector(lambda: db_session)
        signal = DriftSignal(
            model="gpt-4", template_id=None, current_avg=3.8,
            previous_avg=4.5, week_delta=-0.7, severity=DriftSeverity.SIGNIFICANT,
            sample_count=10, detected_at=datetime.now(timezone.utc),
        )
        correction = {"action": "fallback", "from": "gpt-4", "to": "gpt-3.5", "active": True}

        corrector._record(signal, correction)

        row = db_session.execute(text(
            "SELECT content FROM conversation_events WHERE event_type = 'drift_correction' "
            "AND event_id = :eid"
        ), {"eid": f"drift_gpt-4_{int(signal.detected_at.timestamp())}"}).fetchone()

        assert row is not None
        # Must be valid JSON, not Python repr
        parsed = json.loads(row[0])
        assert parsed["action"] == "fallback"
        assert parsed["active"] is True  # JSON true, not Python "True"

    def test_regression_gate_selector_uses_json(self, db_session):
        """_apply_change_to_sandbox must use json.dumps for SELECTOR changes."""
        import inspect
        from core.evaluation.regression_gate import RegressionGate

        source = inspect.getsource(RegressionGate._apply_change_to_sandbox)
        assert "json.dumps" in source
        assert "str(change_content)" not in source


# ── D4: DriftCorrector._apply_fallback persistence ──────────────


class TestDriftFallbackPersistence:
    """_apply_fallback must commit cfg.is_active = False to DB."""

    def test_fallback_calls_commit(self, db_session):
        """Verify db.commit() is called after disabling the model."""
        from core.evaluation.drift_detector import DriftCorrector, DriftSignal, DriftSeverity, CorrectionAction

        mock_router = Mock()
        mock_cfg = Mock()
        mock_cfg.fallback_to = "gpt-3.5-turbo"
        mock_cfg.is_active = True
        mock_router.get.return_value = mock_cfg

        mock_db = MagicMock()
        corrector = DriftCorrector(lambda: mock_db, router=mock_router)

        signal = DriftSignal(
            model="gpt-4", template_id=None, current_avg=3.8,
            previous_avg=4.5, week_delta=-0.7, severity=DriftSeverity.SIGNIFICANT,
            sample_count=10, detected_at=datetime.now(timezone.utc),
        )

        result = corrector._apply_fallback(signal)

        assert result == CorrectionAction.FALLBACK_MODEL
        assert mock_cfg.is_active is False
        mock_db.commit.assert_called_once()


# ── D5: Narrow exception catch in distributed lock ───────────────


class TestDistributedLockExceptionHandling:
    """_try_acquire must only catch IntegrityError/OperationalError, not all exceptions."""

    def test_gate_trigger_try_acquire_source(self, db_session):
        """GateTrigger._try_acquire must not use bare 'except Exception'."""
        import inspect
        from core.evaluation.gate_trigger import GateTrigger

        source = inspect.getsource(GateTrigger._try_acquire)
        assert "except Exception:" not in source
        assert "IntegrityError" in source

    def test_scheduler_try_acquire_source(self, db_session):
        """GovernanceTaskRunner._try_acquire must not use bare 'except Exception'."""
        import inspect
        from core.context.scheduler import GovernanceTaskRunner

        source = inspect.getsource(GovernanceTaskRunner._try_acquire)
        assert "except Exception:" not in source
        assert "IntegrityError" in source

    def test_connection_error_propagates_in_gate_trigger(self, db_session):
        """A connection error during lock INSERT must propagate, not be silently caught."""
        from core.evaluation.gate_trigger import GateTrigger

        gt = GateTrigger(db_factory=lambda: db_session)

        # Simulate a connection error (not IntegrityError/OperationalError)
        with patch.object(db_session, "add", side_effect=ConnectionError("DB down")):
            with pytest.raises(ConnectionError):
                gt._try_acquire(db_session, "test_lock_conn_err")


# ── D6: SLO multi-agent check ───────────────────────────────────


class TestSLOMultiAgentCheck:
    """SLO weekly check must iterate all agents, not just hardcoded 'dev-agent'."""

    def test_slo_checks_all_agents(self, db_session):
        """Verify SLO monitor is called for each agent returned by _get_agent_ids."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from core.evaluation.slo_monitor import AgentSLOReport, SLOStatus, SLOTarget, SLOSeverity

        engine = MemoryGovernanceEngine(lambda: db_session)

        def make_report(agent_id, **kwargs):
            return AgentSLOReport(
                agent_id=agent_id,
                statuses=[
                    SLOStatus(
                        slo=SLOTarget("quality", "avg_quality", 4.0),
                        current_value=3.5, met=False, burn_rate=2.0,
                        severity=SLOSeverity.WARNING, days_elapsed=7, bad_days=3,
                    ),
                ],
                period_days=7,
                created_at=datetime.now(timezone.utc),
            )

        with patch.object(engine, "_get_agent_ids", return_value=["agent-a", "agent-b", "agent-c"]):
            with patch("core.evaluation.slo_monitor.SLOMonitor.check_agent", side_effect=make_report) as mock_check:
                result = engine.run_weekly_tasks()

        # Must be called for all 3 agents
        assert mock_check.call_count == 3
        called_agents = {call.args[0] for call in mock_check.call_args_list}
        assert called_agents == {"agent-a", "agent-b", "agent-c"}
        # 3 agents × 1 violation each = 3
        assert result["slo_violations"] == 3

    def test_slo_fallback_when_no_agents(self, db_session):
        """When no agents in DB, falls back to checking 'dev-agent'."""
        from core.context.lifecycle import MemoryGovernanceEngine
        from core.evaluation.slo_monitor import AgentSLOReport, SLOStatus, SLOTarget, SLOSeverity

        engine = MemoryGovernanceEngine(lambda: db_session)

        report = AgentSLOReport(
            agent_id="dev-agent",
            statuses=[
                SLOStatus(
                    slo=SLOTarget("quality", "avg_quality", 4.0),
                    current_value=4.5, met=True, burn_rate=0.5,
                    severity=SLOSeverity.OK, days_elapsed=7, bad_days=0,
                ),
            ],
            period_days=7,
            created_at=datetime.now(timezone.utc),
        )

        with patch.object(engine, "_get_agent_ids", return_value=[]):
            with patch("core.evaluation.slo_monitor.SLOMonitor.check_agent", return_value=report) as mock_check:
                result = engine.run_weekly_tasks()

        mock_check.assert_called_once_with("dev-agent", period_days=7)
        assert result["slo_violations"] == 0
