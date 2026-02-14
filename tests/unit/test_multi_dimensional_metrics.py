"""Test multi-dimensional learning signal extraction and metric recording."""

import pytest
from datetime import datetime, timezone
from uuid_utils import uuid7

from api.models import SkillExecutionMetric, SkillSelectionEvent
from core.skills.self_improving_selector import SelfImprovingSelector
from core.skills.learning_signals import SignalType, SignalThresholds


class TestMetricRecording:
    """Test automatic metric recording during skill execution."""
    
    def test_record_execution_metrics_directly(self, db):
        """Test that _record_execution_metrics writes to database."""
        from core.agent.executor import AgentExecutor
        from core.skills.registry import SkillRegistry
        from core.skills.mocking import MockMode
        
        # Setup
        registry = SkillRegistry(db)
        executor = AgentExecutor(db, registry, mode=MockMode.PRODUCTION)
        
        # Record metrics directly
        executor._record_execution_metrics(
            skill_name="test_skill",
            session_id="test-session",
            execution_time_ms=1500,
            execution_cost=0.05,
            success=True,
            error_msg=None,
        )
        
        # Verify metrics were recorded
        metrics = db.query(SkillExecutionMetric).filter(
            SkillExecutionMetric.session_id == "test-session"
        ).all()
        
        assert len(metrics) == 1
        metric = metrics[0]
        assert metric.skill_name == "test_skill"
        assert metric.execution_time_ms == 1500
        assert metric.execution_cost == 0.05
        assert metric.success in (True, 1)  # Database may return 1 instead of True
        assert metric.error_message is None


class TestSignalExtraction:
    """Test extraction of all 4 signal types."""
    
    def test_extract_slow_execution_signal(self, db):
        """Test extraction of SLOW_EXECUTION signal."""
        selector = SelfImprovingSelector(
            session=db,
            thresholds=SignalThresholds(slow_execution_ms=5000)
        )
        
        # Create event with slow execution
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id="test-session",
            user_query="Test query",
            context_snapshot="snap",
            available_skills=["skill1"],
            selected_skills=["skill1"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_time_ms=10000,  # 10 seconds - slow!
            created_at=datetime.now(timezone.utc),
        )
        db.add(event)
        db.commit()
        
        # Extract failures
        failures = selector.get_recent_failures(days=1)
        
        assert len(failures) >= 1
        slow_event = next((f for f in failures if f["execution_time_ms"] == 10000), None)
        assert slow_event is not None
        assert slow_event["execution_time_ms"] > selector.thresholds.slow_execution_ms
    
    def test_extract_high_cost_signal(self, db):
        """Test extraction of HIGH_COST signal."""
        selector = SelfImprovingSelector(
            session=db,
            thresholds=SignalThresholds(high_cost_usd=0.10)
        )
        
        # Create event with high cost
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id="test-session",
            user_query="Test query",
            context_snapshot="snap",
            available_skills=["skill1"],
            selected_skills=["skill1"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            execution_cost=0.50,  # $0.50 - expensive!
            created_at=datetime.now(timezone.utc),
        )
        db.add(event)
        db.commit()
        
        # Extract failures
        failures = selector.get_recent_failures(days=1)
        
        assert len(failures) >= 1
        expensive_event = next((f for f in failures if f["execution_cost"] == 0.50), None)
        assert expensive_event is not None
        assert expensive_event["execution_cost"] > selector.thresholds.high_cost_usd
    
    def test_extract_low_satisfaction_signal(self, db):
        """Test extraction of LOW_SATISFACTION signal."""
        selector = SelfImprovingSelector(
            session=db,
            thresholds=SignalThresholds(low_satisfaction=3)
        )
        
        # Create event with low satisfaction
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id="test-session",
            user_query="Test query",
            context_snapshot="snap",
            available_skills=["skill1"],
            selected_skills=["skill1"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={},
            user_feedback_score=2,  # 2 stars - low satisfaction!
            created_at=datetime.now(timezone.utc),
        )
        db.add(event)
        db.commit()
        
        # Extract failures
        failures = selector.get_recent_failures(days=1)
        
        assert len(failures) >= 1
        low_sat_event = next((f for f in failures if f["user_feedback_score"] == 2), None)
        assert low_sat_event is not None
        assert low_sat_event["user_feedback_score"] < selector.thresholds.low_satisfaction
    
    def test_extract_multiple_signal_types(self, db):
        """Test that all signal types can be extracted together."""
        selector = SelfImprovingSelector(session=db)
        
        # Create events with different signal types
        events = [
            # Wrong skill
            SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id="test-session",
                user_query="Query 1",
                context_snapshot="snap",
                available_skills=["skill1"],
                selected_skills=["skill1"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
                selection_correctness=0,  # Wrong!
                created_at=datetime.now(timezone.utc),
            ),
            # Slow execution
            SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id="test-session",
                user_query="Query 2",
                context_snapshot="snap",
                available_skills=["skill2"],
                selected_skills=["skill2"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
                execution_time_ms=10000,  # Slow
                created_at=datetime.now(timezone.utc),
            ),
            # High cost
            SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id="test-session",
                user_query="Query 3",
                context_snapshot="snap",
                available_skills=["skill3"],
                selected_skills=["skill3"],
                selection_method="llm",
                selection_reasoning="Test",
                candidate_scores={},
                execution_cost=0.50,  # Expensive
                created_at=datetime.now(timezone.utc),
            ),
        ]
        
        for event in events:
            db.add(event)
        db.commit()
        
        # Extract all failures
        failures = selector.get_recent_failures(days=1, limit=100)
        
        # Should get all 3 events
        assert len(failures) >= 3
        
        # Verify each signal type is present
        has_wrong_skill = any(f.get("selection_correctness") == 0 for f in failures)
        has_slow = any((f.get("execution_time_ms") or 0) > 5000 for f in failures)
        has_expensive = any((f.get("execution_cost") or 0) > 0.10 for f in failures)
        
        assert has_wrong_skill
        assert has_slow
        assert has_expensive


class TestLearningFromMetrics:
    """Test learning from execution metrics."""
    
    def test_learn_from_slow_executions(self, db):
        """Test that slow executions generate learnings."""
        selector = SelfImprovingSelector(session=db)
        
        # Create metric for slow execution
        metric = SkillExecutionMetric(
            metric_id=str(uuid7()),
            session_id="test-session",
            skill_name="slow_skill",
            execution_time_ms=10000,  # 10 seconds
            execution_cost=0.01,
            success=True,
            created_at=datetime.now(timezone.utc),
        )
        db.add(metric)
        db.commit()
        
        # Get slow executions
        slow_execs = selector.get_slow_executions(days=1)
        
        assert len(slow_execs) >= 1
        assert slow_execs[0]["skill_name"] == "slow_skill"
        assert slow_execs[0]["execution_time_ms"] == 10000
    
    def test_learn_from_expensive_executions(self, db):
        """Test that expensive executions generate learnings."""
        selector = SelfImprovingSelector(session=db)
        
        # Create metric for expensive execution
        metric = SkillExecutionMetric(
            metric_id=str(uuid7()),
            session_id="test-session",
            skill_name="expensive_skill",
            execution_time_ms=1000,
            execution_cost=0.50,  # $0.50
            success=True,
            created_at=datetime.now(timezone.utc),
        )
        db.add(metric)
        db.commit()
        
        # Get expensive executions
        expensive_execs = selector.get_expensive_executions(days=1)
        
        assert len(expensive_execs) >= 1
        assert expensive_execs[0]["skill_name"] == "expensive_skill"
        assert expensive_execs[0]["execution_cost"] == 0.50
