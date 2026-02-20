"""Tests for unified regression gate."""

import pytest
from datetime import datetime, timezone
from unittest.mock import Mock, patch, MagicMock
from sqlalchemy import text

from core.evaluation.regression_gate import RegressionGate, ChangeType


@pytest.fixture
def setup_tables(db_session):
    """Setup gate_results table (conversation_events already exists from Base.metadata)."""
    # Cleanup any leftover test data from previous runs
    db_session.execute(text("DELETE FROM conversation_events WHERE user_id = 'u1'"))
    db_session.execute(text("DROP TABLE IF EXISTS gate_results"))
    db_session.commit()
    
    # Create gate_results table
    db_session.execute(text("""
        CREATE TABLE gate_results (
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
    yield db_session
    
    # Cleanup: delete test data from conversation_events
    db_session.execute(text("DELETE FROM conversation_events WHERE user_id = 'u1'"))
    db_session.execute(text("DROP TABLE IF EXISTS gate_results"))
    db_session.commit()


@pytest.fixture
def gate(setup_tables):
    """RegressionGate instance."""
    return RegressionGate(db=setup_tables)


class TestGoldenSessionSelection:
    """Test golden session selection logic."""
    
    def test_get_golden_sessions_empty(self, gate, setup_tables):
        """Test golden session selection with no data."""
        sessions = gate._get_golden_sessions(limit=10)
        assert sessions == []
    
    def test_get_golden_sessions_filters_by_quality(self, gate, setup_tables):
        """Test that only high-quality sessions are selected."""
        from core.events.event_logger import EventLogger
        from uuid_utils import uuid7
        
        logger = EventLogger(setup_tables)
        
        # Insert high-quality session
        for i in range(3):
            event = logger.create_user_query(
                user_id="u1",
                session_id="s1",
                content=f"query {i}",
            )
            # Update quality score
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        # Insert low-quality session
        for i in range(3):
            event = logger.create_user_query(
                user_id="u1",
                session_id="s2",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 2.0, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        sessions = gate._get_golden_sessions(limit=10)
        
        # Only s1 should be selected (quality_score >= 4.0)
        assert len(sessions) == 1
        assert sessions[0]["session_id"] == "s1"
        assert sessions[0]["avg_score"] >= 4.0
    
    def test_get_golden_sessions_requires_multi_turn(self, gate, setup_tables):
        """Test that only multi-turn sessions are selected."""
        from core.events.event_logger import EventLogger
        
        logger = EventLogger(setup_tables)
        
        # Session with 2 events (should not be selected)
        for i in range(2):
            event = logger.create_user_query(
                user_id="u1",
                session_id="s1",
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
                user_id="u1",
                session_id="s2",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 4.5, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        sessions = gate._get_golden_sessions(limit=10)
        
        # Neither has >= 3 events
        assert len(sessions) == 0
    
    def test_get_golden_sessions_orders_by_score(self, gate, setup_tables):
        """Test that sessions are ordered by quality score."""
        from core.events.event_logger import EventLogger
        
        logger = EventLogger(setup_tables)
        
        # Session s1 with score 4.0
        for i in range(3):
            event = logger.create_user_query(
                user_id="u1",
                session_id="s1",
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
                user_id="u1",
                session_id="s2",
                content=f"query {i}",
            )
            setup_tables.execute(text("""
                UPDATE conversation_events 
                SET quality_score = 5.0, training_eligible = 1
                WHERE event_id = :event_id
            """), {"event_id": event.event_id})
        
        setup_tables.commit()
        
        sessions = gate._get_golden_sessions(limit=10)
        
        # s2 should come first (higher score)
        assert len(sessions) == 2
        assert sessions[0]["session_id"] == "s2"
        assert sessions[0]["avg_score"] > sessions[1]["avg_score"]


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
        
        logger = EventLogger(setup_tables)
        
        # Create a golden session
        for i in range(3):
            event = logger.create_user_query(
                user_id="u1",
                session_id="s1",
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
        """Test that _rollback_learnings() deletes records from the specified time window."""
        from core.skills.pipeline import SkillPipeline
        from unittest.mock import Mock
        
        # Setup selector_learnings table
        setup_tables.execute(text("DROP TABLE IF EXISTS selector_learnings"))
        setup_tables.execute(text("""
            CREATE TABLE selector_learnings (
                learning_id VARCHAR(36) PRIMARY KEY,
                query_pattern VARCHAR(255),
                wrong_skills TEXT,
                correct_skills TEXT,
                improvement_score DOUBLE,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP
            )
        """))
        setup_tables.commit()
        
        # Insert test data with different timestamps
        setup_tables.execute(text("""
            INSERT INTO selector_learnings 
            (learning_id, query_pattern, wrong_skills, correct_skills, improvement_score, created_at)
            VALUES
            ('l1', 'pattern1', '[]', '["skill1"]', 0.8, DATE_SUB(UTC_TIMESTAMP(), INTERVAL 2 DAY)),
            ('l2', 'pattern2', '[]', '["skill2"]', 0.9, DATE_SUB(UTC_TIMESTAMP(), INTERVAL 5 DAY)),
            ('l3', 'pattern3', '[]', '["skill3"]', 0.7, DATE_SUB(UTC_TIMESTAMP(), INTERVAL 10 DAY))
        """))
        setup_tables.commit()
        
        # Create pipeline and call rollback
        mock_improver = Mock()
        pipeline = SkillPipeline(setup_tables, mock_improver)
        pipeline._rollback_learnings(days=7)
        
        # Verify only records within 7 days are deleted
        result = setup_tables.execute(text("SELECT learning_id FROM selector_learnings ORDER BY learning_id"))
        remaining = [row[0] for row in result]
        
        assert "l3" in remaining  # 10 days old, should remain
        assert "l1" not in remaining  # 2 days old, should be deleted
        assert "l2" not in remaining  # 5 days old, should be deleted
        
        # Cleanup
        setup_tables.execute(text("DROP TABLE IF EXISTS selector_learnings"))
        setup_tables.commit()


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
        
        logger = EventLogger(setup_tables)
        
        # Create a golden session
        for i in range(3):
            event = logger.create_user_query(
                user_id="u1",
                session_id="s1",
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
        
        logger = EventLogger(setup_tables)
        
        # Create a golden session
        for i in range(3):
            event = logger.create_user_query(
                user_id="u1",
                session_id="s1",
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
