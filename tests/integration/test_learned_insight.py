"""Integration tests for learned insight (Phase 4: data-driven procedural memory).

Tests use real DB to verify:
- skill_selection_events agent_id column is populated
- execution_success column is populated by backfill
- _get_learned_insight returns data-driven insight after threshold
- _get_learned_insight returns baseline below threshold
- Every field written to skill_selection_events is verified
"""

from datetime import datetime, timezone

import pytest
from uuid_utils import uuid7

from api.models.skill import SkillSelectionEvent
from core.context.prompt_assembler import PromptAssembler


@pytest.fixture
def assembler(db_factory):
    return PromptAssembler(db_factory=db_factory)


@pytest.fixture(autouse=True)
def cleanup(db_session):
    yield
    db_session.query(SkillSelectionEvent).delete()
    db_session.commit()


def _insert_events(db, agent_id, count, success_count):
    """Insert count events, first success_count with execution_success=1, rest=0."""
    for i in range(count):
        db.add(
            SkillSelectionEvent(
                event_id=str(uuid7()),
                session_id=f"sess-{i}",
                agent_id=agent_id,
                user_query=f"test query {i}",
                selected_skills=["tool_a", "tool_b"],
                skill_name="tool_a",
                skill_version="1.0.0",
                selection_method="llm_tool_choice",
                execution_success=1 if i < success_count else 0,
                execution_time_ms=100 + i,
            )
        )
    db.commit()


class TestLearnedInsightRealDB:
    """Real-DB tests for _get_learned_insight."""

    def test_below_threshold_returns_baseline(self, assembler, db_session):
        """< 50 interactions → baseline insight."""
        _insert_events(db_session, "agent-alpha", count=30, success_count=25)

        result = assembler._get_learned_insight("agent-alpha", "specialist")

        assert (
            result == "I focus deeply on my domain but may need to delegate cross-domain questions."
        )

    def test_just_below_threshold_returns_baseline(self, assembler, db_session):
        """Exactly 49 interactions → still baseline (boundary)."""
        _insert_events(db_session, "agent-49", count=49, success_count=40)

        result = assembler._get_learned_insight("agent-49", "default")

        assert (
            result
            == "I'm still learning about my strengths and weaknesses. I'll improve as we work together."
        )

    def test_at_threshold_returns_data_driven(self, assembler, db_session):
        """Exactly 50 interactions → data-driven insight."""
        _insert_events(db_session, "agent-beta", count=50, success_count=40)

        result = assembler._get_learned_insight("agent-beta", "specialist")

        assert (
            result == "Based on recent history: 80% skill selection accuracy over 50 interactions."
        )

    def test_above_threshold_accuracy(self, assembler, db_session):
        """100 interactions, 90 success → 90% accuracy."""
        _insert_events(db_session, "agent-gamma", count=100, success_count=90)

        result = assembler._get_learned_insight("agent-gamma", "default")

        assert (
            result == "Based on recent history: 90% skill selection accuracy over 100 interactions."
        )

    def test_no_agent_id_returns_baseline(self, assembler, db_session):
        """agent_id=None → baseline, even if DB has data for other agents."""
        # Insert data that would trigger data-driven path if agent_id were used
        _insert_events(db_session, "agent-decoy", count=100, success_count=90)

        result = assembler._get_learned_insight(None, "orchestrator")

        assert (
            result == "I break down tasks and delegate to specialists rather than solving directly."
        )

    def test_unknown_agent_type_returns_default(self, assembler):
        """Unknown agent type → default insight."""
        result = assembler._get_learned_insight(None, "unknown_type")

        assert "still learning" in result

    def test_agent_isolation(self, assembler, db_session):
        """Agent A's data doesn't leak into Agent B's insight."""
        _insert_events(db_session, "agent-A", count=60, success_count=50)
        _insert_events(db_session, "agent-B", count=10, success_count=5)

        result_a = assembler._get_learned_insight("agent-A", "specialist")
        result_b = assembler._get_learned_insight("agent-B", "specialist")

        assert (
            result_a
            == "Based on recent history: 83% skill selection accuracy over 60 interactions."
        )
        # Agent B below threshold → baseline
        assert (
            result_b
            == "I focus deeply on my domain but may need to delegate cross-domain questions."
        )

    def test_zero_success_rate(self, assembler, db_session):
        """All failures → 0% accuracy."""
        _insert_events(db_session, "agent-fail", count=50, success_count=0)

        result = assembler._get_learned_insight("agent-fail", "default")

        assert (
            result == "Based on recent history: 0% skill selection accuracy over 50 interactions."
        )

    def test_old_data_excluded_by_30day_window(self, assembler, db_session):
        """Events older than 30 days are excluded from insight calculation."""
        from datetime import timedelta

        agent = "agent-old"
        # Naive UTC to match DB column convention (func.now() stores naive).
        old_time = datetime.now(timezone.utc).replace(tzinfo=None) - timedelta(days=60)

        # 60 old events (outside 30-day window)
        for i in range(60):
            db_session.add(
                SkillSelectionEvent(
                    event_id=str(uuid7()),
                    session_id=f"sess-old-{i}",
                    agent_id=agent,
                    user_query=f"old query {i}",
                    skill_name="tool_a",
                    selection_method="llm",
                    execution_success=1,
                    created_at=old_time,
                )
            )
        # 10 recent events (inside window)
        for i in range(10):
            db_session.add(
                SkillSelectionEvent(
                    event_id=str(uuid7()),
                    session_id=f"sess-new-{i}",
                    agent_id=agent,
                    user_query=f"new query {i}",
                    skill_name="tool_a",
                    selection_method="llm",
                    execution_success=1,
                )
            )
        db_session.commit()

        # Only 10 recent events count → below threshold → baseline
        result = assembler._get_learned_insight(agent, "default")
        assert "still learning" in result


class TestSkillSelectionEventFields:
    """Verify every field written to skill_selection_events is persisted correctly."""

    def test_all_fields_persisted(self, db_session):
        """Insert a SkillSelectionEvent and verify every column."""
        eid = str(uuid7())
        before = datetime.now(timezone.utc).replace(tzinfo=None, microsecond=0)

        db_session.add(
            SkillSelectionEvent(
                event_id=eid,
                session_id="sess-field-test",
                agent_id="agent-field-test",
                user_query="How do I deploy to production?",
                selected_skills=["deploy_tool", "config_tool"],
                skill_name="deploy_tool",
                skill_version="2.1.0",
                selection_method="llm_tool_choice",
                candidate_scores={"deploy_tool": 0.95, "config_tool": 0.72},
                execution_result={"status": "success", "output": "deployed"},
                execution_success=1,
                execution_time_ms=342,
                execution_cost=0.0023,
                user_feedback_score=5,
                selection_correctness=1,
                correction_suggestion={"suggested": "deploy_tool", "reason": "correct"},
            )
        )
        db_session.commit()

        # Re-read from DB to verify persistence
        row = (
            db_session.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == eid).one()
        )

        assert row.event_id == eid
        assert row.session_id == "sess-field-test"
        assert row.agent_id == "agent-field-test"
        assert row.user_query == "How do I deploy to production?"
        assert row.selected_skills == ["deploy_tool", "config_tool"]
        assert row.skill_name == "deploy_tool"
        assert row.skill_version == "2.1.0"
        assert row.selection_method == "llm_tool_choice"
        assert row.candidate_scores == {"deploy_tool": 0.95, "config_tool": 0.72}
        assert row.execution_result == {"status": "success", "output": "deployed"}
        assert row.execution_success == 1
        assert row.execution_time_ms == 342
        assert abs(row.execution_cost - 0.0023) < 1e-6
        assert row.user_feedback_score == 5
        assert row.selection_correctness == 1
        assert row.correction_suggestion == {"suggested": "deploy_tool", "reason": "correct"}
        assert row.created_at is not None
        assert row.created_at >= before

    def test_agent_id_nullable(self, db_session):
        """agent_id can be NULL (pipeline write path doesn't have it)."""
        eid = str(uuid7())
        db_session.add(
            SkillSelectionEvent(
                event_id=eid,
                session_id="sess-no-agent",
                agent_id=None,
                user_query="test",
                skill_name="tool_x",
                selection_method="pipeline",
            )
        )
        db_session.commit()

        row = (
            db_session.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == eid).one()
        )

        assert row.agent_id is None
        assert row.session_id == "sess-no-agent"
        assert row.skill_name == "tool_x"

    def test_execution_success_null_excluded_from_insight(self, assembler, db_session):
        """Events without execution_success are not counted in insight calculation."""
        agent = "agent-partial"
        # 30 events WITH execution data
        for i in range(30):
            db_session.add(
                SkillSelectionEvent(
                    event_id=str(uuid7()),
                    session_id=f"sess-{i}",
                    agent_id=agent,
                    user_query=f"q{i}",
                    skill_name="t",
                    selection_method="llm",
                    execution_success=1,
                )
            )
        # 50 events WITHOUT execution data (execution_success=NULL)
        for i in range(50):
            db_session.add(
                SkillSelectionEvent(
                    event_id=str(uuid7()),
                    session_id=f"sess-null-{i}",
                    agent_id=agent,
                    user_query=f"q{i}",
                    skill_name="t",
                    selection_method="llm",
                    execution_success=None,
                )
            )
        db_session.commit()

        # Only 30 have execution data → below threshold → baseline
        result = assembler._get_learned_insight(agent, "default")
        assert "still learning" in result


class TestTurnHooksAgentId:
    """Verify turn_hooks writes agent_id to skill_selection_events."""

    def test_record_skill_selection_with_agent_id(self, db_factory, db_session):
        from core.agent.turn_hooks import TurnHooks

        hooks = TurnHooks(db_factory)
        tool_calls = [{"function": {"name": "read_file", "arguments": "{}"}}]

        eid = hooks.record_skill_selection(
            session_id="sess-hook-test",
            user_content="read the config",
            tool_calls=tool_calls,
            agent_id="dev-agent",
        )

        assert eid is not None
        row = (
            db_session.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == eid).one()
        )

        assert row.agent_id == "dev-agent"
        assert row.session_id == "sess-hook-test"
        assert row.user_query == "read the config"
        assert row.skill_name == "read_file"
        assert row.selected_skills == ["read_file"]
        assert row.selection_method == "llm_tool_choice"

    def test_record_skill_selection_without_agent_id(self, db_factory, db_session):
        from core.agent.turn_hooks import TurnHooks

        hooks = TurnHooks(db_factory)
        tool_calls = [{"function": {"name": "search", "arguments": "{}"}}]

        eid = hooks.record_skill_selection(
            session_id="sess-no-aid",
            user_content="find bugs",
            tool_calls=tool_calls,
        )

        assert eid is not None
        row = (
            db_session.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == eid).one()
        )

        assert row.agent_id is None
        assert row.skill_name == "search"

    def test_backfill_sets_execution_success(self, db_factory, db_session):
        """backfill_selection_metrics sets execution_success=1 on the latest event."""
        from core.agent.turn_hooks import TurnHooks

        hooks = TurnHooks(db_factory)
        tool_calls = [{"function": {"name": "deploy", "arguments": "{}"}}]

        eid = hooks.record_skill_selection(
            "sess-backfill",
            "deploy app",
            tool_calls,
            agent_id="agent-x",
        )
        hooks.backfill_selection_metrics("sess-backfill", tool_calls, elapsed_ms=250)

        row = (
            db_session.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == eid).one()
        )

        assert row.execution_success == 1
        assert row.execution_time_ms == 250

    def test_backfill_targets_latest_unfilled_event(self, db_factory, db_session):
        """With multiple events, backfill only updates the latest unfilled one."""
        from core.agent.turn_hooks import TurnHooks

        hooks = TurnHooks(db_factory)
        tc = [{"function": {"name": "build", "arguments": "{}"}}]

        eid1 = hooks.record_skill_selection("sess-multi", "first", tc, agent_id="a")
        # Backfill first event so it has execution_time_ms set
        hooks.backfill_selection_metrics("sess-multi", tc, elapsed_ms=100)

        eid2 = hooks.record_skill_selection("sess-multi", "second", tc, agent_id="a")
        # Backfill should target eid2 (latest with execution_time_ms=NULL)
        hooks.backfill_selection_metrics("sess-multi", tc, elapsed_ms=200)

        row1 = (
            db_session.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == eid1).one()
        )
        row2 = (
            db_session.query(SkillSelectionEvent).filter(SkillSelectionEvent.event_id == eid2).one()
        )

        assert row1.execution_time_ms == 100
        assert row2.execution_time_ms == 200
        assert row1.execution_success == 1
        assert row2.execution_success == 1
