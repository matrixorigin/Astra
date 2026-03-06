"""Tests for unified regression gate.

Covers:
- ORM-based golden session selection with real DB
- Full field-level verification of persisted gate results
- End-to-end gate execution with ground truth DB checks
- Parallel-safe (no global DELETE, uid-scoped data)
"""

import json
from datetime import datetime, timezone
from unittest.mock import MagicMock, Mock, patch

import pytest
from sqlalchemy import text

from api.models.agent import Event
from api.models.evaluation import GateResult
from core.evaluation.regression_gate import ChangeType, RegressionGate

pytestmark = pytest.mark.slow


# ── Fixtures ──────────────────────────────────────────────────────────


@pytest.fixture
def setup_tables(db_session):
    """Ensure tables exist and provide isolated test uid."""
    from api.database import init_db
    from core.utils.id_generator import generate_id
    uid = generate_id()

    init_db()

    db_session._test_uid = uid
    db_session._test_gate_ids = []
    yield db_session

    # Scoped cleanup — only this test's data
    db_session.execute(text(
        "DELETE FROM agent_events WHERE user_id = :uid"
    ), {"uid": uid})
    if db_session._test_gate_ids:
        for gid in db_session._test_gate_ids:
            db_session.execute(text(
                "DELETE FROM eval_gate_results WHERE gate_id = :gid"
            ), {"gid": gid})
    db_session.commit()


@pytest.fixture
def gate(setup_tables):
    """RegressionGate instance."""
    return RegressionGate(lambda: setup_tables)


def _sid(uid: str, tag: str) -> str:
    """Build a session_id that fits VARCHAR(36): uid[:28] + '_' + tag[:7]."""
    return f"{uid[:28]}_{tag[:7]}"


def _insert_golden_events(db, uid, session_id, count, quality_score):
    """Helper: insert events with quality score.

    session_id must be <= 36 chars (VARCHAR(36) column).
    """
    from core.events.event_logger import EventLogger
    logger = EventLogger.from_session(db)
    for i in range(count):
        event = logger.create_user_query(
            user_id=uid, session_id=session_id, content=f"query {i}",
        )
        db.execute(text("""
            UPDATE agent_events
            SET quality_score = :qs, training_eligible = 1
            WHERE event_id = :eid
        """), {"qs": quality_score, "eid": event.event_id})
    db.commit()


# ── Golden Session Selection (ORM) ───────────────────────────────────


class TestGoldenSessionSelection:

    def test_empty_returns_no_own_sessions(self, gate, setup_tables):
        uid = setup_tables._test_uid
        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        assert own == []

    def test_filters_by_quality_threshold(self, gate, setup_tables):
        uid = setup_tables._test_uid
        sid_high = _sid(uid, "high")
        sid_low = _sid(uid, "low")
        _insert_golden_events(setup_tables, uid, sid_high, 3, 4.5)
        _insert_golden_events(setup_tables, uid, sid_low, 3, 2.0)

        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]

        assert len(own) == 1
        assert own[0]["session_id"] == sid_high
        assert own[0]["avg_score"] >= 4.0
        assert own[0]["event_count"] == 3

    def test_requires_multi_turn(self, gate, setup_tables):
        uid = setup_tables._test_uid
        _insert_golden_events(setup_tables, uid, _sid(uid, "short"), 2, 4.5)

        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        assert len(own) == 0

    def test_orders_by_score_descending(self, gate, setup_tables):
        uid = setup_tables._test_uid
        sid1 = _sid(uid, "s1")
        sid2 = _sid(uid, "s2")
        _insert_golden_events(setup_tables, uid, sid1, 3, 4.0)
        _insert_golden_events(setup_tables, uid, sid2, 3, 5.0)

        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]

        assert len(own) == 2
        assert own[0]["session_id"] == sid2
        assert own[0]["avg_score"] > own[1]["avg_score"]

    def test_result_dict_has_all_fields(self, gate, setup_tables):
        """Verify every field in the returned dict."""
        uid = setup_tables._test_uid
        _insert_golden_events(setup_tables, uid, _sid(uid, "s1"), 3, 4.5)

        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        assert len(own) == 1

        s = own[0]
        assert isinstance(s["session_id"], str) and s["session_id"]
        assert isinstance(s["user_id"], str) and s["user_id"] == uid
        assert isinstance(s["avg_score"], float) and s["avg_score"] >= 4.0
        assert isinstance(s["event_count"], int) and s["event_count"] >= 3


# ── Metrics Computation ──────────────────────────────────────────────


class TestMetricsComputation:

    def test_empty(self, gate):
        m = gate._compute_metrics([], [])
        assert m == {
            "error_rate": 0.0, "score_delta": 0.0,
            "avg_original_score": 0.0, "avg_replay_score": 0.0,
            "total_sessions": 0, "failed_sessions": 0,
        }

    def test_all_success(self, gate):
        golden = [
            {"session_id": "s1", "user_id": "u1", "avg_score": 4.5, "event_count": 3},
            {"session_id": "s2", "user_id": "u1", "avg_score": 4.0, "event_count": 3},
        ]
        replays = [
            {"session_id": "s1", "replay_status": "completed", "failed": 0},
            {"session_id": "s2", "replay_status": "completed", "failed": 0},
        ]
        m = gate._compute_metrics(golden, replays)
        assert m["error_rate"] == 0.0
        assert m["total_sessions"] == 2
        assert m["failed_sessions"] == 0
        assert m["score_delta"] == 0.0  # replay maintains original

    def test_with_failures(self, gate):
        golden = [
            {"session_id": "s1", "user_id": "u1", "avg_score": 4.5, "event_count": 3},
            {"session_id": "s2", "user_id": "u1", "avg_score": 4.0, "event_count": 3},
        ]
        replays = [
            {"session_id": "s1", "replay_status": "completed", "failed": 0},
            {"session_id": "s2", "replay_status": "failed", "failed": 1},
        ]
        m = gate._compute_metrics(golden, replays)
        assert m["error_rate"] == 0.5
        assert m["failed_sessions"] == 1


# ── Decision Logic ───────────────────────────────────────────────────


class TestDecisionLogic:

    def test_pass(self, gate):
        v, r = gate._make_decision(
            {"error_rate": 0.02, "score_delta": 0.1}, 0.05, -0.1,
        )
        assert v == "pass"
        assert "threshold" in r

    def test_fail_error_rate(self, gate):
        v, r = gate._make_decision(
            {"error_rate": 0.10, "score_delta": 0.1}, 0.05, -0.1,
        )
        assert v == "fail"
        assert "error_rate" in r

    def test_fail_score_regression(self, gate):
        v, r = gate._make_decision(
            {"error_rate": 0.02, "score_delta": -0.2}, 0.05, -0.1,
        )
        assert v == "fail"
        assert "score_delta" in r


# ── Gate Execution (mocked replay) ───────────────────────────────────


class TestGateExecution:

    @patch.object(RegressionGate, '_get_golden_sessions')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    def test_no_golden_sessions_returns_skip(self, _apply, _snap, mock_golden, gate):
        mock_golden.return_value = []
        result = gate.validate_change(
            change_type=ChangeType.PROMPT,
            change_id="test@v1",
            change_content={"content": "test"},
        )
        assert result["verdict"] == "skip"
        assert result["reason"] == "no_golden_sessions_available"
        assert result["sessions_tested"] == 0

    @patch.object(RegressionGate, '_get_golden_sessions')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_success_flow(self, mock_sb_cls, _apply, _snap, mock_golden, gate):
        mock_golden.return_value = [
            {"session_id": "s1", "user_id": "u1", "avg_score": 4.5, "event_count": 3},
        ]
        _snap.return_value = "snapshot_123"
        mock_sb_cls.return_value = MagicMock()

        with patch('api.services.replay_service.ReplayService') as mock_rp_cls:
            mock_rp = MagicMock()
            mock_rp.replay_session.return_value = {
                "status": "completed", "events_replayed": 3,
                "result": {"successful": 3, "failed": 0},
            }
            mock_rp_cls.return_value = mock_rp

            result = gate.validate_change(
                change_type=ChangeType.PROMPT,
                change_id="test@v1",
                change_content={"content": "test"},
                golden_session_count=1,
            )

        assert result["verdict"] == "pass"
        assert result["sessions_tested"] == 1
        assert "metrics" in result
        # ReplayService receives callable db_factory
        mock_rp_cls.assert_called_once()
        assert callable(mock_rp_cls.call_args[0][0])
        # Sandbox created and deleted
        mock_sb_cls.return_value.create.assert_called_once()
        mock_sb_cls.return_value.delete.assert_called_once()


# ── Gate Result Persistence (ORM, field-level) ───────────────────────


class TestGateResultPersistence:
    """End-to-end: _record_gate_result → DB → get_gate_history with every field verified."""

    def test_record_and_retrieve_all_fields(self, gate, setup_tables):
        """Verify EVERY field persisted and retrieved correctly."""
        uid = setup_tables._test_uid
        gate_id = f"gate_{uid[:12]}"
        setup_tables._test_gate_ids.append(gate_id)

        gate_result = {
            "gate_id": gate_id,
            "change_type": "prompt",
            "change_id": "code_review@v3",
            "verdict": "pass",
            "reason": "all_metrics_within_threshold",
            "sessions_tested": 42,
            "snapshot_id": "snapshot_20260306_080000",
            "metrics": {
                "error_rate": 0.02,
                "score_delta": 0.15,
                "avg_original_score": 4.3,
                "avg_replay_score": 4.45,
                "total_sessions": 42,
                "failed_sessions": 1,
            },
            "replay_results": [],
            "created_at": datetime.now(timezone.utc).isoformat(),
        }

        gate._record_gate_result(gate_result)

        # Ground truth: re-query from DB via ORM (not from return value)
        saved = setup_tables.query(GateResult).filter_by(gate_id=gate_id).first()
        assert saved is not None

        # Verify EVERY column
        assert saved.gate_id == gate_id
        assert saved.change_type == "prompt"
        assert saved.change_id == "code_review@v3"
        assert saved.snapshot_used == "snapshot_20260306_080000"
        assert saved.sessions_tested == 42
        assert abs(saved.error_rate - 0.02) < 1e-6
        assert abs(saved.score_delta - 0.15) < 1e-6
        assert saved.passed == 1
        assert saved.metrics is not None
        parsed_metrics = json.loads(saved.metrics)
        assert parsed_metrics["error_rate"] == 0.02
        assert parsed_metrics["total_sessions"] == 42
        assert saved.created_at is not None

    def test_record_failed_gate(self, gate, setup_tables):
        """Verify failed verdict persists passed=0."""
        uid = setup_tables._test_uid
        gate_id = f"fail_{uid[:12]}"
        setup_tables._test_gate_ids.append(gate_id)

        gate._record_gate_result({
            "gate_id": gate_id,
            "change_type": "skill",
            "change_id": "linter@v2",
            "verdict": "fail",
            "reason": "error_rate too high",
            "sessions_tested": 10,
            "snapshot_id": None,
            "metrics": {"error_rate": 0.15, "score_delta": -0.3},
            "replay_results": [],
            "created_at": datetime.now(timezone.utc).isoformat(),
        })

        saved = setup_tables.query(GateResult).filter_by(gate_id=gate_id).first()
        assert saved is not None
        assert saved.passed == 0
        assert saved.change_type == "skill"
        assert saved.change_id == "linter@v2"
        assert saved.snapshot_used is None
        assert abs(saved.error_rate - 0.15) < 1e-6
        assert abs(saved.score_delta - (-0.3)) < 1e-6

    def test_get_gate_history_returns_persisted(self, gate, setup_tables):
        """get_gate_history returns what was persisted, all fields correct."""
        uid = setup_tables._test_uid
        g1 = f"hist1_{uid[:8]}"
        g2 = f"hist2_{uid[:8]}"
        setup_tables._test_gate_ids.extend([g1, g2])

        for gid, ct, cid, passed, er, sd in [
            (g1, "prompt", "p@v1", 1, 0.01, 0.2),
            (g2, "skill", "s@v2", 0, 0.12, -0.3),
        ]:
            gate._record_gate_result({
                "gate_id": gid, "change_type": ct, "change_id": cid,
                "verdict": "pass" if passed else "fail", "reason": "test",
                "sessions_tested": 5, "snapshot_id": f"snap_{gid}",
                "metrics": {"error_rate": er, "score_delta": sd},
                "replay_results": [],
                "created_at": datetime.now(timezone.utc).isoformat(),
            })

        history = gate.get_gate_history(limit=100)
        own = {h["gate_id"]: h for h in history if h["gate_id"] in (g1, g2)}

        assert len(own) == 2

        h1 = own[g1]
        assert h1["change_type"] == "prompt"
        assert h1["change_id"] == "p@v1"
        assert h1["passed"] is True
        assert abs(h1["error_rate"] - 0.01) < 1e-6
        assert abs(h1["score_delta"] - 0.2) < 1e-6
        assert h1["sessions_tested"] == 5
        assert h1["snapshot_used"] == f"snap_{g1}"
        assert h1["created_at"] is not None

        h2 = own[g2]
        assert h2["change_type"] == "skill"
        assert h2["passed"] is False
        assert abs(h2["error_rate"] - 0.12) < 1e-6


# ── Golden Session DB Ground Truth ───────────────────────────────────


class TestGoldenSessionDBGroundTruth:
    """Verify golden session query uses ORM and returns correct DB state."""

    def test_events_persisted_via_orm_are_found(self, gate, setup_tables):
        """Events inserted via EventLogger are found by ORM query."""
        uid = setup_tables._test_uid
        sid = _sid(uid, "golden")
        _insert_golden_events(setup_tables, uid, sid, 4, 4.8)

        # Verify events exist in DB via ORM
        count = setup_tables.query(Event).filter(
            Event.session_id == sid, Event.user_id == uid,
        ).count()
        assert count == 4

        # Verify golden selection finds them
        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["session_id"] == sid]
        assert len(own) == 1
        assert own[0]["event_count"] == 4
        assert abs(own[0]["avg_score"] - 4.8) < 0.01

    def test_no_side_effects_on_other_users(self, gate, setup_tables):
        """Golden session query only returns sessions matching criteria, not other users' data."""
        uid = setup_tables._test_uid
        _insert_golden_events(setup_tables, uid, _sid(uid, "s1"), 3, 4.5)

        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        assert len(own) == 1
        # No other user's sessions leaked into our results
        for s in sessions:
            assert s["avg_score"] >= 4.0
            assert s["event_count"] >= 3


# ── Sandbox Change Application ───────────────────────────────────────


class TestNewChangeTypes:

    def test_change_type_context_budget_exists(self):
        assert ChangeType.CONTEXT_BUDGET.value == "context_budget"

    def test_change_type_knowledge_exists(self):
        assert ChangeType.KNOWLEDGE.value == "knowledge"

    def test_apply_context_budget_to_sandbox(self):
        _mock_db = Mock()
        g = RegressionGate.__new__(RegressionGate)
        g._db_factory = lambda: _mock_db
        g._apply_change_to_sandbox(
            "test_sb", ChangeType.CONTEXT_BUDGET,
            "context_budget_ratios", {"debugging": {"logs": 0.50}},
        )
        sql = _mock_db.execute.call_args[0][0].text
        assert "context_budget_ratios" in sql
        assert "test_sb.configs" in sql

    def test_apply_knowledge_quarantine(self):
        _mock_db = Mock()
        g = RegressionGate.__new__(RegressionGate)
        g._db_factory = lambda: _mock_db
        g._apply_change_to_sandbox(
            "test_sb", ChangeType.KNOWLEDGE,
            "quarantine_e1", {"entry_id": "e1", "action": "quarantine"},
        )
        sql = _mock_db.execute.call_args[0][0].text
        assert "confidence = 0.0" in sql
        assert "test_sb.sk_knowledge_entries" in sql

    def test_apply_knowledge_restore(self):
        _mock_db = Mock()
        g = RegressionGate.__new__(RegressionGate)
        g._db_factory = lambda: _mock_db
        g._apply_change_to_sandbox(
            "test_sb", ChangeType.KNOWLEDGE,
            "restore_e1", {"entry_id": "e1", "action": "restore", "confidence": 0.9},
        )
        params = _mock_db.execute.call_args[0][1]
        assert params["confidence"] == 0.9

    def test_apply_knowledge_missing_entry_id_raises(self):
        g = RegressionGate.__new__(RegressionGate)
        g._db_factory = lambda: Mock()
        with pytest.raises(ValueError, match="entry_id"):
            g._apply_change_to_sandbox(
                "test_sb", ChangeType.KNOWLEDGE, "bad", {"action": "quarantine"},
            )


class TestSkillTableName:

    def test_uses_skills_registry_table(self):
        _mock_db = Mock()
        g = RegressionGate.__new__(RegressionGate)
        g._db_factory = lambda: _mock_db
        g._apply_change_to_sandbox(
            "test_sb", ChangeType.SKILL, "code_review@v2",
            {"name": "code_review", "version": "2.0.0", "definition": {}},
        )
        sql = _mock_db.execute.call_args[0][0].text
        assert "test_sb.skills_registry" in sql
        assert "skill_name" in sql
        assert "skill_definition" in sql


# ── Sandbox Name Validation ──────────────────────────────────────────


class TestSandboxNameValidation:

    def test_rejects_sql_injection(self):
        from core.evaluation.regression_gate import _validate_sandbox_name
        for bad in ["'; DROP TABLE --", "a b", "foo'bar", "x;y"]:
            with pytest.raises(ValueError, match="Invalid sandbox name"):
                _validate_sandbox_name(bad)

    def test_accepts_valid_names(self):
        from core.evaluation.regression_gate import _validate_sandbox_name
        for name in ["gate_abc12345", "test-sb", "my_sandbox"]:
            _validate_sandbox_name(name)

    def test_apply_change_rejects_bad_name(self):
        g = RegressionGate.__new__(RegressionGate)
        g._db_factory = lambda: Mock()
        with pytest.raises(ValueError, match="Invalid sandbox name"):
            g._apply_change_to_sandbox(
                "bad name!", ChangeType.PROMPT, "test", {"content": "x"},
            )


# ── Sandbox Cleanup ──────────────────────────────────────────────────


class TestSandboxCleanup:

    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_deleted_on_failure(self, mock_sb_cls, _snap, _apply, gate, setup_tables):
        uid = setup_tables._test_uid
        _insert_golden_events(setup_tables, uid, _sid(uid, "s1"), 3, 4.5)
        _snap.return_value = "snap"
        mock_sb = MagicMock()
        mock_sb_cls.return_value = mock_sb

        with patch('api.services.replay_service.ReplayService') as mock_rp_cls:
            mock_rp_cls.return_value.replay_session.side_effect = Exception("boom")
            with pytest.raises(Exception, match="boom"):
                gate.validate_change(
                    ChangeType.PROMPT, "test", {"content": "x"}, golden_session_count=1,
                )

        mock_sb.delete.assert_called_once()

    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_deleted_on_success(self, mock_sb_cls, _snap, _apply, gate, setup_tables):
        uid = setup_tables._test_uid
        _insert_golden_events(setup_tables, uid, _sid(uid, "s1"), 3, 4.5)
        _snap.return_value = "snap"
        mock_sb = MagicMock()
        mock_sb_cls.return_value = mock_sb

        with patch('api.services.replay_service.ReplayService') as mock_rp_cls:
            mock_rp_cls.return_value.replay_session.return_value = {
                "status": "completed", "events_replayed": 3,
                "result": {"successful": 3, "failed": 0},
            }
            gate.validate_change(
                ChangeType.PROMPT, "test", {"content": "x"}, golden_session_count=1,
            )

        mock_sb.create.assert_called_once()
        mock_sb.delete.assert_called_once()


# ── Selector Change Validation ───────────────────────────────────────


class TestSelectorChangeValidation:

    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_selector_change(self, mock_sb_cls, _snap, _apply, gate, setup_tables):
        uid = setup_tables._test_uid
        _insert_golden_events(setup_tables, uid, _sid(uid, "s1"), 3, 4.5)
        _snap.return_value = "snap"
        mock_sb_cls.return_value = MagicMock()

        with patch('api.services.replay_service.ReplayService') as mock_rp_cls:
            mock_rp_cls.return_value.replay_session.return_value = {
                "status": "completed", "events_replayed": 3,
                "result": {"successful": 3, "failed": 0},
            }
            result = gate.validate_change(
                ChangeType.SELECTOR, "selector_v2",
                {"learning_rate": 0.01}, golden_session_count=1,
            )

        assert result["verdict"] in ["pass", "fail"]
        assert result["change_type"] == "selector"
        assert result["change_id"] == "selector_v2"
        assert callable(mock_rp_cls.call_args[0][0])


# ── Pollution Gated Quarantine ───────────────────────────────────────


class TestPollutionGatedQuarantine:

    def test_gate_pass(self):
        from core.context.pollution import PollutionDetector
        detector = PollutionDetector(lambda: Mock())
        detector.quarantine_entry = Mock(return_value=True)
        with patch("core.evaluation.regression_gate.RegressionGate") as mock_gate:
            mock_gate.return_value.validate_change.return_value = {"verdict": "pass"}
            result = detector.quarantine_with_validation("e1", "high", "bad")
        assert result["verdict"] == "pass"
        detector.quarantine_entry.assert_called_once_with("e1", "high", "bad")

    def test_gate_fail(self):
        from core.context.pollution import PollutionDetector
        detector = PollutionDetector(lambda: Mock())
        detector.quarantine_entry = Mock()
        with patch("core.evaluation.regression_gate.RegressionGate") as mock_gate:
            mock_gate.return_value.validate_change.return_value = {"verdict": "fail", "reason": "regression"}
            result = detector.quarantine_with_validation("e1", "high")
        assert result["verdict"] == "fail"
        detector.quarantine_entry.assert_not_called()

    def test_gate_unavailable(self):
        from core.context.pollution import PollutionDetector
        detector = PollutionDetector(lambda: Mock())
        detector.quarantine_entry = Mock(return_value=True)
        with patch("core.evaluation.regression_gate.RegressionGate", side_effect=Exception("no gate")):
            result = detector.quarantine_with_validation("e1", "medium")
        assert result["verdict"] == "skipped"
        detector.quarantine_entry.assert_called_once()
