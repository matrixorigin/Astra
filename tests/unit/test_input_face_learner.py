"""Tests for InputFaceLearner — unified meta-learning across input faces.

Covers:
- Prompt face: delegates to PromptOptimizer
- Context budget face: detects truncation, adjusts ratios
- Knowledge face: detects stale entries, quarantines
- Audit trail: learning events recorded
- Error handling: graceful degradation per face
"""

import pytest
from datetime import datetime, timezone
from unittest.mock import Mock, MagicMock, patch

from core.learning.input_face_learner import (
    InputFaceLearner,
    InputFace,
    DiagnosisResult,
)


@pytest.fixture
def mock_db():
    db = MagicMock()
    db.execute.return_value.first.return_value = None
    db.execute.return_value.fetchall.return_value = []
    return db


@pytest.fixture
def mock_llm():
    return Mock()


@pytest.fixture
def learner(mock_db, mock_llm):
    return InputFaceLearner(lambda: mock_db, mock_llm)


class TestDiagnoseAndFix:
    def test_returns_empty_when_no_issues(self, learner):
        results = learner.diagnose_and_fix(days=7)
        assert results == []

    def test_runs_all_faces_by_default(self, learner, mock_db):
        # 3 faces, each returns None (no issues)
        results = learner.diagnose_and_fix(days=7)
        # Should have queried DB for each face
        assert mock_db.execute.call_count >= 3

    def test_runs_specific_faces(self, learner, mock_db):
        results = learner.diagnose_and_fix(days=7, faces=[InputFace.KNOWLEDGE])
        # Only knowledge face queried
        assert len(results) == 0  # no stale entries

    def test_error_in_one_face_doesnt_block_others(self, learner, mock_db):
        """If prompt face throws, knowledge face still runs."""
        call_count = [0]
        original_execute = mock_db.execute

        def side_effect(*args, **kwargs):
            call_count[0] += 1
            if call_count[0] == 1:
                raise RuntimeError("prompt query failed")
            return original_execute(*args, **kwargs)

        mock_db.execute.side_effect = side_effect
        results = learner.diagnose_and_fix(days=7)
        # At least one error result
        errors = [r for r in results if r.error]
        assert len(errors) >= 1


class TestPromptFace:
    def test_no_action_when_no_low_rated_templates(self, learner, mock_db):
        mock_db.execute.return_value.first.return_value = None
        result = learner._handle_prompt(days=7, dry_run=False)
        assert result is None

    def test_triggers_optimizer_when_low_rated(self, learner, mock_db):
        # Simulate: template 'system_general' has 5 low-rated cases, avg 1.5
        mock_db.execute.return_value.first.return_value = ("system_general", 5, 1.5)

        with patch("core.context.prompt_optimizer.PromptOptimizer") as MockOpt:
            mock_opt = MockOpt.return_value
            mock_opt.optimize.return_value = Mock(
                old_version="1.0",
                new_version="1.1",
                diagnosis="Too vague",
                activated=True,
                gate_verdict="pass",
                error=None,
            )
            result = learner._handle_prompt(days=7, dry_run=False)

        assert result is not None
        assert result.input_face == InputFace.PROMPT
        assert result.applied is True
        assert "system_general" in result.bottleneck

    def test_optimizer_exception_returns_partial_result(self, learner, mock_db):
        """PromptOptimizer crash returns DiagnosisResult with error, not exception."""
        mock_db.execute.return_value.first.return_value = ("broken_tpl", 4, 1.2)

        with patch("core.context.prompt_optimizer.PromptOptimizer") as MockOpt:
            MockOpt.return_value.optimize.side_effect = RuntimeError("LLM down")
            result = learner._handle_prompt(days=7, dry_run=False)

        assert result is not None
        assert result.error == "LLM down"
        assert result.applied is False
        assert "broken_tpl" in result.bottleneck

    def test_dry_run_does_not_activate(self, learner, mock_db):
        mock_db.execute.return_value.first.return_value = ("tpl", 3, 1.8)

        with patch("core.context.prompt_optimizer.PromptOptimizer") as MockOpt:
            mock_opt = MockOpt.return_value
            mock_opt.optimize.return_value = Mock(
                old_version="1.0",
                new_version="1.1",
                diagnosis="Needs work",
                activated=False,
                gate_verdict="dry_run",
                error=None,
            )
            result = learner._handle_prompt(days=7, dry_run=True)

        assert result.applied is False
        assert result.gate_verdict == "dry_run"


class TestContextBudgetFace:
    def test_no_action_when_no_truncation(self, learner, mock_db):
        mock_db.execute.return_value.first.return_value = None
        result = learner._handle_context_budget(days=7, dry_run=False)
        assert result is None

    def test_proposes_adjustment(self, learner, mock_db):
        # Simulate: 'code_review' has 40% truncation
        mock_db.execute.return_value.first.return_value = ("code_review", 100, 40, 2.0)

        result = learner._handle_context_budget(days=7, dry_run=True)

        assert result is not None
        assert result.input_face == InputFace.CONTEXT_BUDGET
        assert "truncation" in result.bottleneck
        assert result.proposal is not None
        assert "proposed" in result.proposal

    def test_budget_adjustment_preserves_sum(self, learner):
        proposal = learner._propose_budget_adjustment("code_review", 0.4)
        proposed = proposal["proposed"]
        total = sum(proposed.values())
        assert abs(total - 1.0) < 0.01

    def test_budget_boosts_smallest_section(self, learner):
        """Adjustment boosts the smallest section (most likely truncated)."""
        proposal = learner._propose_budget_adjustment("code_review", 0.5)
        current = proposal["current"]
        proposed = proposal["proposed"]
        # Find smallest and largest in current
        smallest = min(current, key=current.get)
        largest = max(current, key=current.get)
        # Smallest should have grown, largest should have shrunk
        assert proposed[smallest] >= current[smallest]
        assert proposed[largest] <= current[largest]

    def test_budget_lock_serializes_concurrent_access(self, learner, mock_db):
        """_budget_lock prevents concurrent read-modify-write races."""
        import threading

        mock_db.execute.return_value.first.return_value = ("code_review", 100, 40, 2.0)
        results = []
        errors = []

        def run():
            try:
                r = learner._handle_context_budget(days=7, dry_run=True)
                if r:
                    results.append(r)
            except Exception as e:
                errors.append(e)

        threads = [threading.Thread(target=run) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert not errors
        # All threads should complete without exception
        assert len(results) == 4


class TestKnowledgeFace:
    def test_no_action_when_no_stale(self, learner, mock_db):
        mock_db.execute.return_value.first.return_value = (0,)
        result = learner._handle_knowledge(days=7, dry_run=False)
        assert result is None

    def test_diagnose_and_quarantine_use_same_threshold(self, learner, mock_db):
        """Both queries use _STALE_CONFIDENCE to avoid count mismatch."""
        call_count = [0]

        def execute_side_effect(*args, **kwargs):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.first.return_value = (3,)
            elif call_count[0] == 2:
                result.rowcount = 3
            else:
                result.first.return_value = None
            return result

        mock_db.execute.side_effect = execute_side_effect
        learner._handle_knowledge(days=7, dry_run=False)

        # Both calls should use the same threshold parameter
        calls = mock_db.execute.call_args_list
        for call in calls[:2]:
            params = call[0][1] if len(call[0]) > 1 else call[1].get("params", {})
            if "threshold" in params:
                assert params["threshold"] == learner._STALE_CONFIDENCE

    def test_quarantines_stale_entries(self, learner, mock_db):
        # Call 1: stale count query returns 5
        # Call 2: UPDATE returns rowcount=3
        # Call 3: INSERT audit event (from _quarantine_and_record)
        call_count = [0]

        def execute_side_effect(*args, **kwargs):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.first.return_value = (5,)
            elif call_count[0] == 2:
                result.rowcount = 3
            else:
                result.first.return_value = None
            return result

        mock_db.execute.side_effect = execute_side_effect

        result = learner._handle_knowledge(days=7, dry_run=False)

        assert result is not None
        assert result.input_face == InputFace.KNOWLEDGE
        assert result.applied is True
        assert result.proposal["count"] == 3
        # Audit INSERT happened in same transaction (single commit)
        mock_db.commit.assert_called()

    def test_quarantine_and_audit_are_atomic(self, learner, mock_db):
        """If audit INSERT fails, quarantine UPDATE is also rolled back."""
        call_count = [0]

        def execute_side_effect(*args, **kwargs):
            call_count[0] += 1
            result = MagicMock()
            if call_count[0] == 1:
                result.first.return_value = (3,)  # stale count
            elif call_count[0] == 2:
                result.rowcount = 2  # quarantine UPDATE
            elif call_count[0] == 3:
                raise RuntimeError("audit INSERT failed")  # audit fails
            return result

        mock_db.execute.side_effect = execute_side_effect

        result = learner._handle_knowledge(days=7, dry_run=False)

        # Entire transaction rolled back — applied should be False
        assert result.applied is False
        assert result.proposal["count"] == 0
        mock_db.rollback.assert_called()

    def test_dry_run_does_not_quarantine(self, learner, mock_db):
        mock_db.execute.return_value.first.return_value = (10,)

        result = learner._handle_knowledge(days=7, dry_run=True)

        assert result is not None
        assert result.applied is False
        assert result.gate_verdict == "dry_run"
        # Should NOT have called UPDATE
        calls = [str(c) for c in mock_db.execute.call_args_list]
        assert not any("UPDATE" in c for c in calls)


class TestAuditTrail:
    def test_learning_event_recorded(self, learner, mock_db):
        result = DiagnosisResult(
            input_face=InputFace.PROMPT,
            bottleneck="test",
            proposal={"action": "test"},
            applied=True,
            gate_verdict="pass",
        )
        learner._record_learning_event(InputFace.PROMPT, result)

        # Should have called execute (INSERT) and commit
        assert mock_db.execute.call_count >= 1
        mock_db.commit.assert_called()

    def test_audit_failure_does_not_propagate(self, learner, mock_db):
        mock_db.execute.side_effect = RuntimeError("db down")
        result = DiagnosisResult(
            input_face=InputFace.KNOWLEDGE,
            bottleneck="test",
        )
        # Should not raise
        learner._record_learning_event(InputFace.KNOWLEDGE, result)
        mock_db.rollback.assert_called()
