"""Integration tests for cost signal closed-loop and pipeline observability.

Design intent verified:
- Executor emits HIGH_COST with actual_usd/actual_tokens → pipeline buffers → DB persisted
- Executor backfills execution_cost/time/success to skill_selection_events via ORM
- pipeline.stats() aggregates per-skill metrics from real DB rows
- End-to-end: selection → execution → backfill → stats reflects correct values
"""

from datetime import datetime, timezone
from unittest.mock import MagicMock, patch

import pytest
from uuid_utils import uuid7

from api.models.skill import (
    SkillExecutionMetric,
    SkillLearningSignal,
    SkillSelectionEvent,
)
from core.skills.learning_signals import SignalThresholds, SignalType
from core.skills.pipeline import SkillPipeline


@pytest.fixture(autouse=True)
def _clean(db):
    """Clean tables before and after each test."""
    for model in (SkillLearningSignal, SkillExecutionMetric, SkillSelectionEvent):
        db.query(model).delete(synchronize_session=False)
    db.commit()
    yield
    for model in (SkillLearningSignal, SkillExecutionMetric, SkillSelectionEvent):
        db.query(model).delete(synchronize_session=False)
    db.commit()


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_selection_event(db, **overrides) -> tuple[str, str]:
    """Insert a SkillSelectionEvent, return (event_id, session_id) as plain strings."""
    now = datetime.now(timezone.utc).replace(tzinfo=None)
    event_id = overrides.pop("event_id", str(uuid7()))
    session_id = overrides.pop("session_id", str(uuid7()))
    defaults = {
        "event_id": event_id,
        "session_id": session_id,
        "user_query": "test query",
        "selected_skills": ["test_skill"],
        "skill_name": "test_skill",
        "selection_method": "semantic",
        "created_at": now,
    }
    defaults.update(overrides)
    evt = SkillSelectionEvent(**defaults)
    db.add(evt)
    db.commit()
    return event_id, session_id


def _make_executor(db, mock_pipeline):
    from core.agent.executor import AgentExecutor

    return AgentExecutor(
        db_factory=lambda: db,
        registry=MagicMock(),
        pipeline=mock_pipeline,
    )


# ---------------------------------------------------------------------------
# 1. HIGH_COST signal emission — verify every field in signal payload
# ---------------------------------------------------------------------------


class TestHighCostSignal:
    """Executor emits HIGH_COST with correct payload when cost > threshold."""

    def test_emits_high_cost_above_threshold(self, db):
        """HIGH_COST signal emitted with all required fields."""
        thresholds = SignalThresholds()
        cost = thresholds.high_cost_usd + 0.05

        mock_pipeline = MagicMock()
        executor = _make_executor(db, mock_pipeline)

        with patch.object(executor, "execute_skill", return_value={"cost": cost}), \
             patch.object(executor, "_backfill_selection_event"):
            executor.execute_skill_with_feedback(
                skill_name="expensive_skill",
                params={},
                session_id=str(uuid7()),
                selection_event_id=str(uuid7()),
                extra_feedback_data={"actual_tokens": 8500},
            )

        calls = mock_pipeline.record_feedback.call_args_list
        assert len(calls) == 2, f"Expected EXECUTION_TIME + HIGH_COST, got {len(calls)}"

        # --- EXECUTION_TIME signal: verify every field ---
        _, et_signal, et_data = calls[0][0]
        assert et_signal == SignalType.EXECUTION_TIME
        assert et_data["skill"] == "expensive_skill"
        assert et_data["actual_usd"] == cost
        assert et_data["actual_tokens"] == 8500  # merged from extra_feedback_data
        assert "ms" in et_data
        assert et_data["ms"] > 0

        # --- HIGH_COST signal: verify every field ---
        _, hc_signal, hc_data = calls[1][0]
        assert hc_signal == SignalType.HIGH_COST
        assert hc_data["skill"] == "expensive_skill"
        assert hc_data["actual_usd"] == cost
        assert hc_data["actual_tokens"] == 8500
        assert hc_data["threshold_usd"] == thresholds.high_cost_usd

    def test_no_high_cost_at_threshold(self, db):
        """Exactly at threshold → no HIGH_COST (must exceed, not equal)."""
        thresholds = SignalThresholds()
        mock_pipeline = MagicMock()
        executor = _make_executor(db, mock_pipeline)

        with patch.object(executor, "execute_skill", return_value={"cost": thresholds.high_cost_usd}), \
             patch.object(executor, "_backfill_selection_event"):
            executor.execute_skill_with_feedback(
                skill_name="borderline",
                params={},
                session_id=str(uuid7()),
                selection_event_id=str(uuid7()),
            )

        calls = mock_pipeline.record_feedback.call_args_list
        assert len(calls) == 1
        assert calls[0][0][1] == SignalType.EXECUTION_TIME

    def test_no_high_cost_below_threshold(self, db):
        """Below threshold → only EXECUTION_TIME."""
        mock_pipeline = MagicMock()
        executor = _make_executor(db, mock_pipeline)

        with patch.object(executor, "execute_skill", return_value={"cost": 0.01}), \
             patch.object(executor, "_backfill_selection_event"):
            executor.execute_skill_with_feedback(
                skill_name="cheap",
                params={},
                session_id=str(uuid7()),
                selection_event_id=str(uuid7()),
            )

        calls = mock_pipeline.record_feedback.call_args_list
        assert len(calls) == 1
        assert calls[0][0][1] == SignalType.EXECUTION_TIME
        assert calls[0][0][2]["actual_usd"] == 0.01

    def test_actual_tokens_defaults_to_zero_without_extra_data(self, db):
        """When no extra_feedback_data, actual_tokens defaults to 0."""
        mock_pipeline = MagicMock()
        executor = _make_executor(db, mock_pipeline)

        with patch.object(executor, "execute_skill", return_value={"cost": 0.50}), \
             patch.object(executor, "_backfill_selection_event"):
            executor.execute_skill_with_feedback(
                skill_name="no_tokens",
                params={},
                session_id=str(uuid7()),
                selection_event_id=str(uuid7()),
                # no extra_feedback_data
            )

        hc_data = mock_pipeline.record_feedback.call_args_list[1][0][2]
        assert hc_data["actual_tokens"] == 0

    def test_no_feedback_without_pipeline(self, db):
        """No crash and no feedback signals when pipeline is None."""
        from core.agent.executor import AgentExecutor

        executor = AgentExecutor(
            db_factory=lambda: db,
            registry=MagicMock(),
            pipeline=None,
        )

        with patch.object(executor, "execute_skill", return_value={"cost": 1.0}), \
             patch.object(executor, "_backfill_selection_event"):
            executor.execute_skill_with_feedback(
                skill_name="orphan",
                params={},
                session_id=str(uuid7()),
                selection_event_id=str(uuid7()),
            )

        # Verify no learning signals were persisted
        assert db.query(SkillLearningSignal).count() == 0


# ---------------------------------------------------------------------------
# 2. Backfill — ORM update writes correct fields to skill_selection_events
# ---------------------------------------------------------------------------


class TestBackfillSelectionEvent:
    """_backfill_selection_event updates the correct row via ORM."""

    def test_backfill_updates_all_metric_fields(self, db):
        """After backfill, execution_time_ms/cost/success are set correctly."""
        event_id, session_id = _make_selection_event(db, skill_name="github")

        # Pre-condition: metrics are NULL
        pre = db.query(SkillSelectionEvent).filter_by(event_id=event_id).one()
        assert pre.execution_time_ms is None
        assert pre.execution_cost is None
        assert pre.execution_success is None

        executor = _make_executor(db, MagicMock())
        executor._backfill_selection_event(event_id, time_ms=350, cost=0.07, success=True)

        # Re-query from DB
        db.expire_all()
        saved = db.query(SkillSelectionEvent).filter_by(event_id=event_id).one()
        assert saved.execution_time_ms == 350
        assert saved.execution_cost == pytest.approx(0.07)
        assert saved.execution_success == 1

        # Verify unchanged fields are untouched
        assert saved.skill_name == "github"
        assert saved.user_query == "test query"
        assert saved.session_id == session_id

    def test_backfill_failure_sets_success_zero(self, db):
        """Failed execution → execution_success = 0."""
        event_id, _ = _make_selection_event(db)
        executor = _make_executor(db, MagicMock())
        executor._backfill_selection_event(event_id, time_ms=5000, cost=0.0, success=False)

        db.expire_all()
        saved = db.query(SkillSelectionEvent).filter_by(event_id=event_id).one()
        assert saved.execution_success == 0
        assert saved.execution_time_ms == 5000
        assert saved.execution_cost == pytest.approx(0.0)

    def test_backfill_nonexistent_event_no_crash(self, db):
        """Backfilling a missing event_id is a silent no-op."""
        executor = _make_executor(db, MagicMock())
        executor._backfill_selection_event("nonexistent", time_ms=100, cost=0.0, success=True)
        # No exception


# ---------------------------------------------------------------------------
# 3. pipeline.stats() — ORM aggregation, verify every output field
# ---------------------------------------------------------------------------


class TestPipelineStats:
    """stats() returns correct per-skill aggregation from real DB rows."""

    def test_per_skill_all_fields(self, db):
        """Verify selection_count, success_rate, avg_cost_usd, avg_time_ms."""
        _make_selection_event(db, skill_name="github", execution_success=1, execution_cost=0.02, execution_time_ms=100)
        _make_selection_event(db, skill_name="github", execution_success=1, execution_cost=0.04, execution_time_ms=200)
        _make_selection_event(db, skill_name="github", execution_success=0, execution_cost=0.06, execution_time_ms=300)
        _make_selection_event(db, skill_name="knowledge", execution_success=1, execution_cost=0.10, execution_time_ms=50)

        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False)
        result = pipeline.stats()

        # Top-level structure
        assert "learning" in result
        assert "per_skill" in result

        # github: 3 selections, 2 success, avg cost = (0.02+0.04+0.06)/3
        gh = result["per_skill"]["github"]
        assert gh["selection_count"] == 3
        assert gh["success_rate"] == pytest.approx(2 / 3, abs=0.01)
        assert gh["avg_cost_usd"] == pytest.approx((0.02 + 0.04 + 0.06) / 3, abs=0.001)
        assert gh["avg_time_ms"] == pytest.approx(200.0, abs=0.1)

        # knowledge: 1 selection, 1 success
        kn = result["per_skill"]["knowledge"]
        assert kn["selection_count"] == 1
        assert kn["success_rate"] == 1.0
        assert kn["avg_cost_usd"] == pytest.approx(0.10, abs=0.001)
        assert kn["avg_time_ms"] == pytest.approx(50.0, abs=0.1)

    def test_null_skill_name_excluded(self, db):
        """Rows with skill_name=NULL are excluded from aggregation."""
        _make_selection_event(db, skill_name=None, execution_success=1, execution_cost=0.01, execution_time_ms=10)
        _make_selection_event(db, skill_name="github", execution_success=1, execution_cost=0.05, execution_time_ms=100)

        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False)
        result = pipeline.stats()

        assert len(result["per_skill"]) == 1
        assert "github" in result["per_skill"]

    def test_empty_table(self, db):
        """Empty table → empty per_skill dict."""
        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False)
        result = pipeline.stats()
        assert result["per_skill"] == {}

    def test_learning_disabled_shows_error(self, db):
        """When learning=False, learning section contains error."""
        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False)
        result = pipeline.stats()
        assert result["learning"] == {"error": "Learning disabled"}
        assert "per_skill" in result

    def test_null_cost_excluded_from_avg(self, db):
        """Rows with NULL execution_cost are excluded from AVG (standard SQL)."""
        _make_selection_event(db, skill_name="github", execution_success=1, execution_cost=None, execution_time_ms=100)
        _make_selection_event(db, skill_name="github", execution_success=1, execution_cost=0.10, execution_time_ms=200)

        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False)
        gh = pipeline.stats()["per_skill"]["github"]
        assert gh["selection_count"] == 2
        # SQL AVG ignores NULLs: AVG(NULL, 0.10) = 0.10
        assert gh["avg_cost_usd"] == pytest.approx(0.10, abs=0.001)


# ---------------------------------------------------------------------------
# 4. End-to-end: selection → execute → backfill → stats reflects truth
# ---------------------------------------------------------------------------


class TestCostClosedLoopEndToEnd:
    """Design intent: cost flows from execution through backfill into stats."""

    def test_execute_backfills_then_stats_reflects(self, db):
        """Full loop: create selection event → execute → backfill → stats correct."""
        # Step 1: Create selection event (simulates pipeline.get_tools_schema audit)
        event_id, session_id = _make_selection_event(db, skill_name="expensive_llm")

        # Step 2: Execute with feedback (simulates ChatLoop calling executor)
        mock_pipeline = MagicMock()
        executor = _make_executor(db, mock_pipeline)

        with patch.object(executor, "execute_skill", return_value={"cost": 0.25}):
            executor.execute_skill_with_feedback(
                skill_name="expensive_llm",
                params={},
                session_id=session_id,
                selection_event_id=event_id,
                extra_feedback_data={"actual_tokens": 12000},
            )

        # Step 3: Verify backfill wrote to DB
        db.expire_all()
        saved = db.query(SkillSelectionEvent).filter_by(event_id=event_id).one()
        assert saved.execution_cost == pytest.approx(0.25)
        assert saved.execution_success == 1
        assert saved.execution_time_ms is not None
        assert saved.execution_time_ms >= 0  # mocked execute_skill is near-instant

        # Step 4: Verify stats() reflects the backfilled data
        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False)
        stats = pipeline.stats()
        skill_stats = stats["per_skill"]["expensive_llm"]
        assert skill_stats["selection_count"] == 1
        assert skill_stats["success_rate"] == 1.0
        assert skill_stats["avg_cost_usd"] == pytest.approx(0.25, abs=0.001)

        # Step 5: Verify HIGH_COST signal was emitted with correct payload
        hc_calls = [c for c in mock_pipeline.record_feedback.call_args_list
                     if c[0][1] == SignalType.HIGH_COST]
        assert len(hc_calls) == 1
        hc_data = hc_calls[0][0][2]
        assert hc_data["actual_usd"] == 0.25
        assert hc_data["actual_tokens"] == 12000
        assert hc_data["skill"] == "expensive_llm"

    def test_failed_execution_reflected_in_stats(self, db):
        """Failed execution → success_rate drops, cost still recorded."""
        event_id, session_id = _make_selection_event(db, skill_name="flaky_skill")

        mock_pipeline = MagicMock()
        executor = _make_executor(db, mock_pipeline)

        with patch.object(executor, "execute_skill", side_effect=RuntimeError("boom")), \
             pytest.raises(RuntimeError, match="boom"):
            executor.execute_skill_with_feedback(
                skill_name="flaky_skill",
                params={},
                session_id=session_id,
                selection_event_id=event_id,
            )

        db.expire_all()
        saved = db.query(SkillSelectionEvent).filter_by(event_id=event_id).one()
        assert saved.execution_success == 0
        assert saved.execution_cost == pytest.approx(0.0)
        # Time still recorded even for failures (elapsed before exception)
        assert saved.execution_time_ms is not None
        assert saved.execution_time_ms >= 0

        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=False, learning=False)
        stats = pipeline.stats()["per_skill"]["flaky_skill"]
        assert stats["selection_count"] == 1
        assert stats["success_rate"] == 0.0


# ---------------------------------------------------------------------------
# 5. _record_selection ORM — verify audit event fields
# ---------------------------------------------------------------------------


class TestRecordSelectionORM:
    """_record_selection creates SkillSelectionEvent via ORM with all fields."""

    def test_audit_event_all_fields(self, db):
        """Verify every field written by _record_selection."""
        from datetime import timedelta

        before = datetime.now(timezone.utc).replace(tzinfo=None)

        pipeline = SkillPipeline(lambda: db, llm_client=None, audit=True, learning=False)
        tools = [
            {"function": {"name": "github_create_pr"}, "type": "function"},
            {"function": {"name": "github_list_prs"}, "type": "function"},
        ]
        session_id = str(uuid7())
        event_id = pipeline._record_selection("create a PR", session_id, tools, "semantic")

        after = datetime.now(timezone.utc).replace(tzinfo=None)

        # Re-query from DB
        saved = db.query(SkillSelectionEvent).filter_by(event_id=event_id).one()
        assert saved.event_id == event_id
        assert saved.session_id == session_id
        assert saved.user_query == "create a PR"
        assert saved.selected_skills == ["github_create_pr", "github_list_prs"]
        assert saved.skill_name == "github_create_pr"  # top-ranked
        assert saved.selection_method == "semantic"
        # MatrixOne truncates datetime to seconds; allow ±1s tolerance
        assert before - timedelta(seconds=1) <= saved.created_at <= after + timedelta(seconds=1)

        # Fields not set by _record_selection should be NULL
        assert saved.execution_success is None
        assert saved.execution_cost is None
        assert saved.execution_time_ms is None
        assert saved.agent_id is None
