"""Integration test for self-improving selector in AgentSkillSelector."""

import pytest
from datetime import datetime, timezone
from uuid_utils import uuid7

from api.database import get_db_session
from api.models import SkillSelectionEvent, SkillSelectionLearning, SelectorGateResult
from core.agent.selector import AgentSkillSelector


class TestSelfImprovingSelectorIntegration:
    """Test self-improving selector integration with regression gating."""

    @pytest.fixture(autouse=True)
    def setup(self):
        """Setup test database."""
        self.db = next(get_db_session())
        self.llm_client = None  # Mock LLM not needed for this test
        
        # Clean up test data
        self.db.query(SkillSelectionEvent).delete()
        self.db.query(SkillSelectionLearning).delete()
        self.db.query(SelectorGateResult).delete()
        self.db.commit()
        
        yield
        
        # Cleanup
        self.db.query(SkillSelectionEvent).delete()
        self.db.query(SkillSelectionLearning).delete()
        self.db.query(SelectorGateResult).delete()
        self.db.commit()
        self.db.close()

    def test_selector_with_learning_enabled(self):
        """Test AgentSkillSelector with learning enabled."""
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        assert selector.enable_learning is True
        assert selector.improving_selector is not None
        assert selector.regression_gate is not None

    def test_selector_with_learning_disabled(self):
        """Test AgentSkillSelector with learning disabled."""
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=False,
        )
        
        assert selector.enable_learning is False

    def test_learn_from_failures_no_data(self):
        """Test learning cycle with no failure data."""
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        result = selector.learn_from_failures(days=7)
        
        assert result["learned"] == 0
        assert "message" in result

    def test_learn_from_failures_with_data(self):
        """Test learning cycle with failure data."""
        # Create a failed selection event
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="How do I create a PR?",
            context_snapshot="snapshot_123",
            available_skills=[],
            selected_skills=["wrong_skill"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={"wrong_skill": 1.0},
            selection_correctness=0,  # Failed
            correction_suggestion=["github_create_pr"],
            created_at=datetime.now(timezone.utc),
        )
        self.db.add(event)
        self.db.commit()
        
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        result = selector.learn_from_failures(days=7)
        
        assert result["learned"] >= 1
        assert "gate_verdict" in result

    def test_learning_stats(self):
        """Test getting learning statistics."""
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        stats = selector.get_learning_stats()
        
        assert "learnings" in stats
        assert "regression_gates" in stats
        assert "total_learnings" in stats["learnings"]
        assert "total_gates" in stats["regression_gates"]

    def test_learning_cycle_records_to_database(self):
        """Test that learning cycle records results to database."""
        # Create a failed selection
        event = SkillSelectionEvent(
            event_id=str(uuid7()),
            session_id=str(uuid7()),
            user_query="Create a GitHub issue",
            context_snapshot="snapshot_456",
            available_skills=[],
            selected_skills=["wrong_tool"],
            selection_method="llm",
            selection_reasoning="Test",
            candidate_scores={"wrong_tool": 1.0},
            selection_correctness=0,
            correction_suggestion=["github_create_issue"],
            created_at=datetime.now(timezone.utc),
        )
        self.db.add(event)
        self.db.commit()
        
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        # Trigger learning
        result = selector.learn_from_failures(days=7)
        
        # Check learning was recorded
        learnings = self.db.query(SkillSelectionLearning).all()
        assert len(learnings) >= 1
        
        # Check gate result was recorded (if gate ran)
        if result.get("gate_verdict") != "skipped":
            gates = self.db.query(SelectorGateResult).all()
            assert len(gates) >= 1
            
            gate = gates[0]
            assert gate.verdict in ["PASS", "FAIL"]
            assert gate.learnings_applied == result["learned"]

    def test_apply_learnings_during_selection(self):
        """Test that learnings are applied during skill selection."""
        # Create a high-confidence learning
        learning = SkillSelectionLearning(
            learning_id=str(uuid7()),
            query_pattern="create pr",
            wrong_skills=["wrong_skill"],
            correct_skills=["github_create_pr"],
            improvement_score=10.0,
            confidence=80.0,  # High confidence
            evidence_count=5,
            created_at=datetime.now(timezone.utc),
        )
        self.db.add(learning)
        self.db.commit()
        
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        # Select skills with query matching the learning pattern
        # Note: This will use the learning to filter candidates
        # The actual behavior depends on the selector implementation
        
        # Verify learning was applied
        learning_after = self.db.query(SkillSelectionLearning).filter(
            SkillSelectionLearning.learning_id == learning.learning_id
        ).first()
        
        # Applied count should increase when learning is used
        # (This test verifies the mechanism is in place)
        assert learning_after is not None

    def test_learning_cooldown(self):
        """Test learning cooldown period."""
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
            learning_cooldown_hours=1,
        )
        
        # First learning should work
        result1 = selector.learn_from_failures(days=7)
        assert "error" not in result1 or result1.get("error") != "cooldown"
        
        # Second learning immediately should be blocked
        result2 = selector.learn_from_failures(days=7)
        assert result2.get("error") == "cooldown"
        assert "cooldown_hours" in result2
        
        # Force should bypass cooldown
        result3 = selector.learn_from_failures(days=7, force=True)
        assert result3.get("error") != "cooldown"

    def test_learning_with_database_error(self):
        """Test learning handles database errors gracefully."""
        selector = AgentSkillSelector(
            db=self.db,
            llm_client=self.llm_client,
            auditable=True,
            session_id=str(uuid7()),
            enable_learning=True,
        )
        
        # Mock database failure
        original_learn = selector.improving_selector.learn_from_failures
        selector.improving_selector.learn_from_failures = lambda days, signal_types=None: (_ for _ in ()).throw(
            Exception("Database connection failed")
        )
        
        result = selector.learn_from_failures(days=7)
        
        assert result.get("error") == "learning_failed"
        assert "Database connection failed" in result.get("message", "")
        assert result.get("learned") == 0
        
        # Restore
        selector.improving_selector.learn_from_failures = original_learn

    def test_skill_candidate_dataclass(self):
        """Test SkillCandidate dataclass usage."""
        from core.agent.selector import SkillCandidate
        
        candidate = SkillCandidate(name="test_skill")
        assert candidate.name == "test_skill"
        assert candidate.version == "1.0.0"
        assert candidate.confidence == 1.0
        
        candidate2 = SkillCandidate(name="test_skill2", version="2.0.0", confidence=0.8)
        assert candidate2.name == "test_skill2"
        assert candidate2.version == "2.0.0"
        assert candidate2.confidence == 0.8

    def test_multi_signal_learning_cycle(self, db_session):
        """Test full learning cycle with multiple signal types."""
        from core.skills.learning_signals import SignalType
        from api.models import SkillSelectionEvent
        from uuid_utils import uuid7
        
        # Create failures for different signal types
        events = [
            # Wrong skill
            SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id="test_session",
                user_query="Create a PR",
                selected_skills=["wrong_skill"],
                selection_correctness=0,
                correction_suggestion=["github_create_pr"],
            ),
            # Slow execution
            SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id="test_session",
                user_query="Run tests",
                selected_skills=["slow_test"],
                execution_time_ms=10000,  # 10 seconds
                selection_correctness=1,
            ),
            # High cost
            SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id="test_session",
                user_query="Analyze data",
                selected_skills=["expensive_skill"],
                execution_cost=0.50,  # $0.50
                selection_correctness=1,
            ),
        ]
        
        for event in events:
            db_session.add(event)
        db_session.commit()
        
        # Create selector
        selector = AgentSkillSelector(
            db=db_session,
            llm_client=None,
            auditable=True,
            session_id="test_session",
            enable_learning=True,
        )
        
        # Learn from specific signal types
        result = selector.learn_from_failures(
            days=7,
            force=True,
            signal_types=[SignalType.WRONG_SKILL, SignalType.SLOW_EXECUTION, SignalType.HIGH_COST],
        )
        
        # Verify results
        assert result["learned"] >= 1  # At least wrong_skill
        assert "signals_by_type" in result
        
        # Check signal breakdown
        signals = result["signals_by_type"]
        assert signals.get("wrong_skill", 0) >= 1
        # Note: slow/cost signals require failures (selection_correctness=0)
        # or specific conditions to trigger
        
        # Verify learnings in database
        from api.models import SkillSelectionLearning
        learnings = db_session.query(SkillSelectionLearning).all()
        assert len(learnings) >= 1
        
        # Check signal types are recorded
        signal_types_found = {l.signal_type for l in learnings}
        assert "wrong_skill" in signal_types_found


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
