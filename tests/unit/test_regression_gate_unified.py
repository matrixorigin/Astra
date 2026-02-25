"""Tests for unified regression gate."""

import pytest
from datetime import datetime, timezone
from unittest.mock import Mock, patch, MagicMock

pytestmark = pytest.mark.slow
from sqlalchemy import text

from core.evaluation.regression_gate import RegressionGate, ChangeType


@pytest.fixture
def setup_tables(db_session):
    """Setup gate_results table (conversation_events already exists from Base.metadata)."""
    from core.utils.id_generator import generate_id
    # Use unique user_id to avoid cross-worker contamination in parallel tests
    uid = f"gate_test_{generate_id()[:8]}"

    # Cleanup any leftover test data from previous runs
    db_session.execute(text("DROP TABLE IF EXISTS gate_results"))
    db_session.commit()
    
    # Create gate_results table
    db_session.execute(text("""
        CREATE TABLE IF NOT EXISTS gate_results (
            gate_id VARCHAR(36) PRIMARY KEY,
            change_type VARCHAR(20) NOT NULL,
            change_id VARCHAR(128) NOT NULL,
            snapshot_used VARCHAR(64),
            sessions_tested INT DEFAULT 0,
            error_rate DOUBLE DEFAULT 0.0,
            score_delta DOUBLE DEFAULT 0.0,
            passed TINYINT(1) NOT NULL,
            metrics TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )
    """))
    
    db_session.commit()
    # Stash unique user_id for tests to use
    db_session._test_uid = uid
    yield db_session
    
    # Cleanup: delete only our test data
    db_session.execute(text(
        "DELETE FROM conversation_events WHERE user_id = :uid"
    ), {"uid": uid})
    db_session.execute(text("DROP TABLE IF EXISTS gate_results"))
    db_session.commit()


@pytest.fixture
def gate(setup_tables):
    """RegressionGate instance."""
    return RegressionGate(db=setup_tables)


class TestGoldenSessionSelection:
    """Test golden session selection logic."""
    
    def test_get_golden_sessions_empty(self, gate, setup_tables):
        """Test golden session selection with no data from this test."""
        uid = setup_tables._test_uid
        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        assert own == []
    
    def test_get_golden_sessions_filters_by_quality(self, gate, setup_tables):
        """Test that only high-quality sessions are selected."""
        from core.events.event_logger import EventLogger
        
        uid = setup_tables._test_uid
        logger = EventLogger.from_session(setup_tables)
        
        # Insert high-quality session
        for i in range(3):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s1",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        # Insert low-quality session
        for i in range(3):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s2",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 2.0, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        
        # Only s1 should be selected (quality_score >= 4.0)
        assert len(own) == 1
        assert own[0]["session_id"] == f"{uid}_s1"
        assert own[0]["avg_score"] >= 4.0
    
    def test_get_golden_sessions_requires_multi_turn(self, gate, setup_tables):
        """Test that only multi-turn sessions are selected."""
        from core.events.event_logger import EventLogger
        
        uid = setup_tables._test_uid
        logger = EventLogger.from_session(setup_tables)
        
        # Session with 2 events (should not be selected)
        for i in range(2):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s1",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        # Session with 2 events (should not be selected)
        for i in range(2):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s2",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        
        # Neither has >= 3 events
        assert len(own) == 0
    
    def test_get_golden_sessions_orders_by_score(self, gate, setup_tables):
        """Test that sessions are ordered by quality score."""
        from core.events.event_logger import EventLogger
        
        uid = setup_tables._test_uid
        logger = EventLogger.from_session(setup_tables)
        
        # Session s1 with score 4.0
        for i in range(3):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s1",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.0, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        # Session s2 with score 5.0
        for i in range(3):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s2",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 5.0, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        sessions = gate._get_golden_sessions(limit=100)
        own = [s for s in sessions if s["user_id"] == uid]
        
        # s2 should come first (higher score)
        assert len(own) == 2
        assert own[0]["session_id"] == f"{uid}_s2"
        assert own[0]["avg_score"] > own[1]["avg_score"]


class TestMetricsComputation:
    """Test metrics computation logic."""
    
    def test_compute_metrics_empty(self, gate):
        """Test metrics computation with no results."""
        metrics = gate._compute_metrics([], [])
        
        assert metrics["error_rate"] == 0.0
        assert metrics["score_delta"] == 0.0
        assert metrics["total_sessions"] == 0
    
    def test_compute_metrics_all_success(self, gate):
        """Test metrics computation with all successful replays."""
        golden_sessions = [
            {"session_id": "s1", "user_id": "u1", "avg_score": 4.5, "event_count": 3},
            {"session_id": "s2", "user_id": "u1", "avg_score": 4.0, "event_count": 3},
        ]
        
        replay_results = [
            {"session_id": "s1", "replay_status": "completed", "failed": 0},
            {"session_id": "s2", "replay_status": "completed", "failed": 0},
        ]
        
        metrics = gate._compute_metrics(golden_sessions, replay_results)
        
        assert metrics["error_rate"] == 0.0
        assert metrics["total_sessions"] == 2
        assert metrics["failed_sessions"] == 0
    
    def test_compute_metrics_with_failures(self, gate):
        """Test metrics computation with failed replays."""
        golden_sessions = [
            {"session_id": "s1", "user_id": "u1", "avg_score": 4.5, "event_count": 3},
            {"session_id": "s2", "user_id": "u1", "avg_score": 4.0, "event_count": 3},
        ]
        
        replay_results = [
            {"session_id": "s1", "replay_status": "completed", "failed": 0},
            {"session_id": "s2", "replay_status": "failed", "failed": 1},
        ]
        
        metrics = gate._compute_metrics(golden_sessions, replay_results)
        
        assert metrics["error_rate"] == 0.5  # 1 failed out of 2
        assert metrics["failed_sessions"] == 1


class TestDecisionLogic:
    """Test pass/fail decision logic."""
    
    def test_make_decision_pass(self, gate):
        """Test decision logic with passing metrics."""
        metrics = {
            "error_rate": 0.02,  # Below 5% threshold
            "score_delta": 0.1,  # Positive improvement
        }
        
        verdict, reason = gate._make_decision(
            metrics=metrics,
            error_rate_threshold=0.05,
            score_regression_threshold=-0.1,
        )
        
        assert verdict == "pass"
        assert "threshold" in reason
    
    def test_make_decision_fail_error_rate(self, gate):
        """Test decision logic failing on error rate."""
        metrics = {
            "error_rate": 0.10,  # Above 5% threshold
            "score_delta": 0.1,
        }
        
        verdict, reason = gate._make_decision(
            metrics=metrics,
            error_rate_threshold=0.05,
            score_regression_threshold=-0.1,
        )
        
        assert verdict == "fail"
        assert "error_rate" in reason
    
    def test_make_decision_fail_score_regression(self, gate):
        """Test decision logic failing on score regression."""
        metrics = {
            "error_rate": 0.02,
            "score_delta": -0.2,  # Below -0.1 threshold
        }
        
        verdict, reason = gate._make_decision(
            metrics=metrics,
            error_rate_threshold=0.05,
            score_regression_threshold=-0.1,
        )
        
        assert verdict == "fail"
        assert "score_delta" in reason


class TestGateExecution:
    """Test full gate execution flow."""
    
    @patch.object(RegressionGate, '_get_golden_sessions')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    def test_validate_change_no_golden_sessions(
        self,
        mock_apply,
        mock_snapshot,
        mock_golden,
        gate,
    ):
        """Test gate validation with no golden sessions."""
        mock_golden.return_value = []
        
        result = gate.validate_change(
            change_type=ChangeType.PROMPT,
            change_id="test_prompt@v1",
            change_content={"content": "test"},
        )
        
        assert result["verdict"] == "skip"
        assert result["reason"] == "no_golden_sessions_available"
        assert result["sessions_tested"] == 0
    
    @patch.object(RegressionGate, '_get_golden_sessions')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch('core.evaluation.regression_gate.ReplayService')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_validate_change_success(
        self,
        mock_sandbox_class,
        mock_replay_class,
        mock_apply,
        mock_snapshot,
        mock_golden,
        gate,
        db_session,
    ):
        """Test successful gate validation."""
        # Mock golden sessions
        mock_golden.return_value = [
            {"session_id": "s1", "user_id": "u1", "avg_score": 4.5, "event_count": 3},
        ]
        
        mock_snapshot.return_value = "snapshot_123"
        
        # Mock sandbox
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox
        gate.sandbox = mock_sandbox
        
        # Mock replay service
        mock_replay = MagicMock()
        mock_replay.replay_session.return_value = {
            "status": "completed",
            "events_replayed": 3,
            "result": {"successful": 3, "failed": 0},
        }
        mock_replay_class.return_value = mock_replay
        gate.replay_service = mock_replay
        
        result = gate.validate_change(
            change_type=ChangeType.PROMPT,
            change_id="test_prompt@v1",
            change_content={"content": "test"},
            golden_session_count=1,
        )
        
        assert result["verdict"] == "pass"
        assert result["sessions_tested"] == 1
        assert "metrics" in result
        
        # Verify sandbox was created and deleted
        mock_sandbox.create.assert_called_once()
        mock_sandbox.delete.assert_called_once()


class TestGateHistory:
    """Test gate history retrieval."""
    
    def test_get_gate_history_empty(self, gate, setup_tables):
        """Test gate history with no data."""
        history = gate.get_gate_history(limit=10)
        assert history == []
    
    def test_get_gate_history_returns_results(self, gate, setup_tables):
        """Test gate history returns recorded results."""
        # Insert test data
        setup_tables.execute(text("""
            INSERT INTO gate_results 
            (gate_id, change_type, change_id, sessions_tested, error_rate, score_delta, passed, metrics, created_at)
            VALUES
            ('g1', 'prompt', 'test@v1', 10, 0.02, 0.1, 1, '{}', NOW()),
            ('g2', 'skill', 'test@v2', 20, 0.10, -0.2, 0, '{}', NOW())
        """))
        setup_tables.commit()
        
        history = gate.get_gate_history(limit=10)
        
        assert len(history) == 2
        assert history[0]["gate_id"] in ["g1", "g2"]
        assert history[0]["change_type"] in ["prompt", "skill"]


class TestSelectorChangeValidation:
    """Test selector change type validation."""
    
    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch('core.evaluation.regression_gate.ReplayService')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_validate_selector_change(
        self,
        mock_sandbox_class,
        mock_replay_class,
        mock_snapshot,
        mock_apply,
        gate,
        setup_tables,
    ):
        """Test that ChangeType.SELECTOR is properly validated."""
        from core.events.event_logger import EventLogger
        
        uid = setup_tables._test_uid
        logger = EventLogger.from_session(setup_tables)
        
        # Create a golden session
        for i in range(3):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s1",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        mock_snapshot.return_value = "snapshot_123"
        
        # Mock sandbox
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox
        gate.sandbox = mock_sandbox
        
        # Mock replay service
        mock_replay = MagicMock()
        mock_replay.replay_session.return_value = {
            "status": "completed",
            "events_replayed": 3,
            "result": {"successful": 3, "failed": 0},
        }
        mock_replay_class.return_value = mock_replay
        gate.replay_service = mock_replay
        
        result = gate.validate_change(
            change_type=ChangeType.SELECTOR,
            change_id="selector_v2",
            change_content={"learning_rate": 0.01, "threshold": 0.8},
            golden_session_count=1,
        )
        
        assert result["verdict"] in ["pass", "fail"]
        assert result["change_type"] == "selector"
        assert result["change_id"] == "selector_v2"
        assert "metrics" in result


class TestRollbackMechanism:
    """Test rollback mechanism for failed gates."""
    
    def test_rollback_learnings_deletes_recent_records(self, setup_tables):
        """Test that _rollback_learnings() soft-deletes recent learnings via SelfImprovingSelector."""
        from core.skills.pipeline import SkillPipeline
        from unittest.mock import Mock, patch
        from datetime import timedelta

        mock_improver = Mock()
        mock_improver.rollback_learnings.return_value = 2

        mock_llm = Mock()
        pipeline = SkillPipeline(setup_tables, mock_llm, learning=False)
        pipeline._improver = mock_improver

        pipeline._rollback_learnings(days=7)

        mock_improver.rollback_learnings.assert_called_once()
        call_kwargs = mock_improver.rollback_learnings.call_args
        since = call_kwargs[1]["since"]
        # since should be ~7 days ago
        expected = datetime.now(timezone.utc) - timedelta(days=7)
        assert abs((since - expected).total_seconds()) < 5


class TestSandboxCleanup:
    """Test sandbox cleanup on gate failure."""
    
    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch('core.evaluation.regression_gate.ReplayService')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_sandbox_deleted_on_gate_failure(
        self,
        mock_sandbox_class,
        mock_replay_class,
        mock_snapshot,
        mock_apply,
        gate,
        setup_tables,
    ):
        """Test that sandbox is deleted even when gate fails."""
        from core.events.event_logger import EventLogger
        
        uid = setup_tables._test_uid
        logger = EventLogger.from_session(setup_tables)
        
        # Create a golden session
        for i in range(3):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s1",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        mock_snapshot.return_value = "snapshot_123"
        
        # Mock sandbox
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox
        gate.sandbox = mock_sandbox
        
        # Mock replay service to simulate failure
        mock_replay = MagicMock()
        mock_replay.replay_session.side_effect = Exception("Replay failed")
        mock_replay_class.return_value = mock_replay
        gate.replay_service = mock_replay
        
        try:
            gate.validate_change(
                change_type=ChangeType.PROMPT,
                change_id="test_prompt",
                change_content={"content": "new prompt"},
                golden_session_count=1,
            )
        except Exception:
            pass  # Expected to fail
        
        # Verify sandbox.delete() was called despite the failure
        mock_sandbox.delete.assert_called_once()
    
    @patch.object(RegressionGate, '_apply_change_to_sandbox')
    @patch.object(RegressionGate, '_create_snapshot')
    @patch('core.evaluation.regression_gate.ReplayService')
    @patch('core.evaluation.regression_gate.Sandbox')
    def test_sandbox_deleted_on_successful_gate(
        self,
        mock_sandbox_class,
        mock_replay_class,
        mock_snapshot,
        mock_apply,
        gate,
        setup_tables,
    ):
        """Test that sandbox is deleted after successful gate validation."""
        from core.events.event_logger import EventLogger
        
        uid = setup_tables._test_uid
        logger = EventLogger.from_session(setup_tables)
        
        # Create a golden session
        for i in range(3):
            event = logger.create_user_query(
                user_id=uid,
                session_id=f"{uid}_s1",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        mock_snapshot.return_value = "snapshot_123"
        
        # Mock sandbox
        mock_sandbox = MagicMock()
        mock_sandbox_class.return_value = mock_sandbox
        gate.sandbox = mock_sandbox
        
        # Mock replay service for success
        mock_replay = MagicMock()
        mock_replay.replay_session.return_value = {
            "status": "completed",
            "events_replayed": 3,
            "result": {"successful": 3, "failed": 0},
        }
        mock_replay_class.return_value = mock_replay
        gate.replay_service = mock_replay
        
        result = gate.validate_change(
            change_type=ChangeType.PROMPT,
            change_id="test_prompt",
            change_content={"content": "new prompt"},
            golden_session_count=1,
        )
        
        # Verify sandbox was created and deleted
        mock_sandbox.create.assert_called_once()
        mock_sandbox.delete.assert_called_once()


class TestNewChangeTypes:
    """Tests for CONTEXT_BUDGET and KNOWLEDGE change types."""

    def test_change_type_context_budget_exists(self):
        assert ChangeType.CONTEXT_BUDGET.value == "context_budget"

    def test_change_type_knowledge_exists(self):
        assert ChangeType.KNOWLEDGE.value == "knowledge"

    def test_apply_context_budget_to_sandbox(self, gate, setup_tables):
        """CONTEXT_BUDGET change should execute upsert SQL."""
        gate_mock = RegressionGate.__new__(RegressionGate)
        gate_mock.db = Mock()
        gate_mock.db.execute = Mock()
        gate_mock.db.commit = Mock()
        gate_mock._apply_change_to_sandbox(
            sandbox_name="test_sb",
            change_type=ChangeType.CONTEXT_BUDGET,
            change_id="context_budget_ratios",
            change_content={"debugging": {"logs": 0.50}},
        )
        # Verify execute was called with the right SQL pattern
        call_args = gate_mock.db.execute.call_args
        sql_text = call_args[0][0].text
        assert "context_budget_ratios" in sql_text
        assert "test_sb.configs" in sql_text

    def test_apply_knowledge_quarantine_to_sandbox(self, gate, setup_tables):
        """KNOWLEDGE quarantine should set confidence=0."""
        gate_mock = RegressionGate.__new__(RegressionGate)
        gate_mock.db = Mock()
        gate_mock.db.execute = Mock()
        gate_mock.db.commit = Mock()
        gate_mock._apply_change_to_sandbox(
            sandbox_name="test_sb",
            change_type=ChangeType.KNOWLEDGE,
            change_id="quarantine_entry123",
            change_content={"entry_id": "entry123", "action": "quarantine"},
        )
        sql_text = gate_mock.db.execute.call_args[0][0].text
        assert "confidence = 0.0" in sql_text
        assert "test_sb.sk_knowledge_entries" in sql_text

    def test_apply_knowledge_restore_to_sandbox(self, gate, setup_tables):
        """KNOWLEDGE restore should set confidence to specified value."""
        gate_mock = RegressionGate.__new__(RegressionGate)
        gate_mock.db = Mock()
        gate_mock.db.execute = Mock()
        gate_mock.db.commit = Mock()
        gate_mock._apply_change_to_sandbox(
            sandbox_name="test_sb",
            change_type=ChangeType.KNOWLEDGE,
            change_id="restore_entry123",
            change_content={"entry_id": "entry123", "action": "restore", "confidence": 0.9},
        )
        sql_text = gate_mock.db.execute.call_args[0][0].text
        assert "confidence = :confidence" in sql_text
        params = gate_mock.db.execute.call_args[0][1]
        assert params["confidence"] == 0.9

    def test_apply_knowledge_missing_entry_id_raises(self, gate, setup_tables):
        """KNOWLEDGE change without entry_id should raise ValueError."""
        gate_mock = RegressionGate.__new__(RegressionGate)
        gate_mock.db = Mock()
        gate_mock.db.execute = Mock()
        gate_mock.db.commit = Mock()
        with pytest.raises(ValueError, match="entry_id"):
            gate_mock._apply_change_to_sandbox(
                sandbox_name="test_sb",
                change_type=ChangeType.KNOWLEDGE,
                change_id="bad",
                change_content={"action": "quarantine"},
            )


class TestPollutionGatedQuarantine:
    """Tests for PollutionDetector.quarantine_with_validation."""

    def test_quarantine_with_gate_pass(self):
        from core.context.pollution import PollutionDetector

        db = Mock()
        detector = PollutionDetector(db)
        detector.quarantine_entry = Mock(return_value=True)

        with patch("core.evaluation.regression_gate.RegressionGate") as MockGate:
            MockGate.return_value.validate_change.return_value = {"verdict": "pass"}
            result = detector.quarantine_with_validation("entry1", "high", "bad data")

        assert result["verdict"] == "pass"
        detector.quarantine_entry.assert_called_once_with("entry1", "high", "bad data")

    def test_quarantine_with_gate_fail(self):
        from core.context.pollution import PollutionDetector

        db = Mock()
        detector = PollutionDetector(db)
        detector.quarantine_entry = Mock()

        with patch("core.evaluation.regression_gate.RegressionGate") as MockGate:
            MockGate.return_value.validate_change.return_value = {"verdict": "fail", "reason": "regression"}
            result = detector.quarantine_with_validation("entry1", "high")

        assert result["verdict"] == "fail"
        detector.quarantine_entry.assert_not_called()

    def test_quarantine_with_gate_unavailable(self):
        from core.context.pollution import PollutionDetector

        db = Mock()
        detector = PollutionDetector(db)
        detector.quarantine_entry = Mock(return_value=True)

        with patch("core.evaluation.regression_gate.RegressionGate", side_effect=Exception("no gate")):
            result = detector.quarantine_with_validation("entry1", "medium")

        assert result["verdict"] == "skipped"
        detector.quarantine_entry.assert_called_once()


class TestSkillTableName:
    """SKILL gate must write to skills_registry, not skills."""

    def test_apply_skill_change_uses_skills_registry_table(self):
        """SKILL change should target skills_registry table with correct columns."""
        gate = RegressionGate.__new__(RegressionGate)
        gate.db = Mock()
        gate.db.execute = Mock()
        gate.db.commit = Mock()
        gate._apply_change_to_sandbox(
            sandbox_name="test_sb",
            change_type=ChangeType.SKILL,
            change_id="code_review@v2",
            change_content={"name": "code_review", "version": "2.0.0", "definition": {}},
        )
        sql_text = gate.db.execute.call_args[0][0].text
        assert "test_sb.skills_registry" in sql_text
        assert "skill_name" in sql_text
        assert "skill_definition" in sql_text
