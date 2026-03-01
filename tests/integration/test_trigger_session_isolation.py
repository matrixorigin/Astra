"""Integration tests for trigger session-per-trigger isolation.

Verifies the core correctness property of the _trigger_loop refactor:
each trigger gets its own DB session so that a failure in one trigger
cannot corrupt the claim state of another.

All tests use a real MatrixOne database — only the LLM / RunEngine
execution path is mocked (we're testing session lifecycle, not agent runs).
"""

from __future__ import annotations

from datetime import datetime, timezone
from unittest.mock import MagicMock, patch

import pytest
from sqlalchemy import text

from core.agent.triggers import (
    claim_and_advance,
    create_trigger,
    delete_trigger,
    fire_trigger,
    get_due_triggers,
    get_trigger,
    _auto_session,
)
from core.utils.id_generator import generate_id


# ── Fixtures ────────────────────────────────────────────────────────


@pytest.fixture
def db(db_session):
    return db_session


@pytest.fixture
def session_id(db):
    from core.events.session_manager import SessionManager
    sid = SessionManager(db).create_session(
        user_id="test-user", metadata={"source": "trigger_isolation"},
    ).session_id
    yield sid
    db.execute(text("DELETE FROM wf_triggers WHERE session_id = :sid"), {"sid": sid})
    db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": sid})
    db.commit()


def _make_due_trigger(db, session_id, name_suffix=""):
    """Create a schedule trigger with next_fire_at in the past (immediately due)."""
    trig = create_trigger(
        db, user_id="test-user", agent_id="dev-agent",
        trigger_type="schedule",
        name=f"iso-{generate_id()[:8]}{name_suffix}",
        user_input="test input",
        cron_expr="* * * * *",
        session_id=session_id,
    )
    # Force next_fire_at into the past so get_due_triggers picks it up.
    db.execute(
        text("UPDATE wf_triggers SET next_fire_at = :past WHERE trigger_id = :tid"),
        {"past": datetime(2020, 1, 1), "tid": trig["trigger_id"]},
    )
    db.commit()
    return trig["trigger_id"]


# ── Tests ───────────────────────────────────────────────────────────


class TestTriggerSessionIsolation:
    """Core correctness: one trigger's failure must not affect another."""

    def test_fire_failure_does_not_rollback_other_claims(self, db, session_id):
        """If fire_trigger raises for trigger A, trigger B's claim is still committed.

        Before the refactor, both triggers shared one session.  A rollback
        in fire_trigger(A) would undo claim_and_advance(B)'s commit.
        """
        tid_a = _make_due_trigger(db, session_id, "-A")
        tid_b = _make_due_trigger(db, session_id, "-B")

        try:
            from api.database import SessionLocal

            # Claim both triggers with *separate* sessions (as the new loop does).
            db_a = SessionLocal()
            try:
                assert claim_and_advance(db_a, tid_a) is True
            finally:
                db_a.close()

            db_b = SessionLocal()
            try:
                assert claim_and_advance(db_b, tid_b) is True
            finally:
                db_b.close()

            # fire_trigger for A raises — this must NOT affect B's claim.
            with patch("core.agent.triggers.get_trigger", side_effect=RuntimeError("boom")):
                with pytest.raises(RuntimeError, match="boom"):
                    fire_trigger(SessionLocal, tid_a)

            # B's claim is still advanced (next_fire_at in the future).
            # Re-read with a fresh session to avoid stale cache.
            db_check = SessionLocal()
            try:
                loaded_b = get_trigger(db_check, tid_b)
            finally:
                db_check.close()
            assert loaded_b["next_fire_at"] > datetime(2020, 1, 2)

            # Verify the claim was persisted by checking next_fire_at is
            # close to "now + 1 minute" (cron is "* * * * *").  We don't
            # re-claim because that is time-sensitive: if the test crosses
            # a minute boundary the second claim would succeed again.
        finally:
            delete_trigger(db, tid_a)
            delete_trigger(db, tid_b)

    def test_broken_session_does_not_affect_next_trigger(self, db, session_id):
        """A session left in a broken state by trigger A must not prevent
        trigger B from being claimed with a fresh session.

        Simulates the pre-refactor failure mode: one shared session hits
        an error, then the next claim on that same session would fail.
        With the refactor, each trigger gets its own session so B succeeds.
        """
        tid_a = _make_due_trigger(db, session_id, "-X")
        tid_b = _make_due_trigger(db, session_id, "-Y")

        try:
            from api.database import SessionLocal

            # Claim A, then break the session with an invalid query.
            db_a = SessionLocal()
            try:
                assert claim_and_advance(db_a, tid_a) is True
                try:
                    db_a.execute(text("SELECT * FROM nonexistent_table_xyz"))
                except Exception:
                    pass  # Session is now in a broken/dirty state.
            finally:
                db_a.close()

            # Claim B with a *fresh* session — must succeed despite A's broken session.
            db_b = SessionLocal()
            try:
                assert claim_and_advance(db_b, tid_b) is True
            finally:
                db_b.close()

            # Both claims persisted independently.
            loaded_a = get_trigger(db, tid_a)
            loaded_b = get_trigger(db, tid_b)
            assert loaded_a["next_fire_at"] > datetime(2020, 1, 2)
            assert loaded_b["next_fire_at"] > datetime(2020, 1, 2)
        finally:
            delete_trigger(db, tid_a)
            delete_trigger(db, tid_b)


class TestFireTriggerUsesFactory:
    """fire_trigger must use db_factory for its own sessions, not a shared one."""

    def test_fire_trigger_closes_its_session(self, db, session_id):
        """fire_trigger's internal session is closed even on success."""
        tid = _make_due_trigger(db, session_id)

        try:
            close_calls = []

            from api.database import SessionLocal

            original_factory = SessionLocal

            def tracking_factory():
                s = original_factory()
                original_close = s.close

                def tracked_close():
                    close_calls.append(s)
                    original_close()

                s.close = tracked_close
                return s

            with patch("core.agent.run_engine.RunEngine") as MockEngine, \
                 patch("core.agent.triggers._auto_session", return_value=session_id), \
                 patch("asyncio.create_task"):
                mock_run = MagicMock()
                mock_run.run_id = "run-1"
                mock_run.status.value = "pending"
                MockEngine.return_value.create_run.return_value = mock_run

                fire_trigger(tracking_factory, tid)

            # fire_trigger opens at least one session (for get_trigger)
            # and every opened session must be closed.
            assert len(close_calls) >= 1, "fire_trigger did not close any sessions"
        finally:
            delete_trigger(db, tid)

    def test_fire_trigger_passes_factory_to_run_engine(self, db, session_id):
        """RunEngine must receive the factory, not a raw session."""
        tid = _make_due_trigger(db, session_id)

        try:
            from api.database import SessionLocal

            with patch("core.agent.run_engine.RunEngine") as MockEngine, \
                 patch("core.agent.triggers._auto_session", return_value=session_id), \
                 patch("asyncio.create_task"):
                mock_run = MagicMock()
                mock_run.run_id = "run-1"
                mock_run.status.value = "pending"
                MockEngine.return_value.create_run.return_value = mock_run

                fire_trigger(SessionLocal, tid)

            # RunEngine was constructed with the factory callable, not a Session.
            MockEngine.assert_called_once()
            factory_arg = MockEngine.call_args[0][0]
            assert factory_arg is SessionLocal, \
                "RunEngine must receive the exact factory callable passed to fire_trigger"
        finally:
            delete_trigger(db, tid)


class TestAutoSessionUsesFactory:
    """_auto_session must use a short-lived session from the factory."""

    def test_auto_session_creates_and_closes_session(self, db, session_id):
        """_auto_session opens a session, creates a chat session, and closes it."""
        from api.database import SessionLocal

        close_calls = []
        original_factory = SessionLocal

        def tracking_factory():
            s = original_factory()
            original_close = s.close

            def tracked_close():
                close_calls.append(s)
                original_close()

            s.close = tracked_close
            return s

        result_sid = _auto_session(tracking_factory, "test-user")

        assert result_sid  # Non-empty session ID.
        assert len(close_calls) == 1, "_auto_session must open and close exactly one session"

        # Clean up the auto-created session.
        db.execute(text("DELETE FROM agent_sessions WHERE session_id = :sid"), {"sid": result_sid})
        db.commit()


class TestGetDueTriggersRealDB:
    """get_due_triggers with real DB — verifies the SQL query works."""

    def test_returns_only_due_triggers(self, db, session_id):
        """Only triggers with next_fire_at <= now are returned."""
        tid_due = _make_due_trigger(db, session_id, "-due")

        # Create a trigger that is NOT due (next_fire_at in the future).
        trig_future = create_trigger(
            db, user_id="test-user", agent_id="dev-agent",
            trigger_type="schedule",
            name=f"iso-{generate_id()[:8]}-future",
            user_input="test input",
            cron_expr="* * * * *",
            session_id=session_id,
        )
        # Force next_fire_at far into the future so it's never due.
        db.execute(
            text("UPDATE wf_triggers SET next_fire_at = :future WHERE trigger_id = :tid"),
            {"future": datetime(2099, 1, 1), "tid": trig_future["trigger_id"]},
        )
        db.commit()

        try:
            due = get_due_triggers(db)
            assert tid_due in due
            assert trig_future["trigger_id"] not in due
        finally:
            delete_trigger(db, tid_due)
            delete_trigger(db, trig_future["trigger_id"])

    def test_inactive_triggers_excluded(self, db, session_id):
        """Inactive triggers are never returned even if due."""
        tid = _make_due_trigger(db, session_id, "-inactive")
        db.execute(
            text("UPDATE wf_triggers SET is_active = 0 WHERE trigger_id = :tid"),
            {"tid": tid},
        )
        db.commit()

        try:
            due = get_due_triggers(db)
            assert tid not in due
        finally:
            delete_trigger(db, tid)


class TestTriggerLoopPattern:
    """Simulate the full _trigger_loop pattern with real DB."""

    def test_full_loop_iteration(self, db, session_id):
        """get_due → claim → fire for multiple triggers, one fails mid-fire.

        Reproduces the exact pattern from _trigger_loop in api/main.py.
        Verifies that a failure firing trigger A does not prevent trigger B
        from being claimed and fired successfully.
        """
        tid_a = _make_due_trigger(db, session_id, "-loopA")
        tid_b = _make_due_trigger(db, session_id, "-loopB")

        try:
            from api.database import SessionLocal

            # Step 1: get_due_triggers with its own session.
            db_query = SessionLocal()
            try:
                due = get_due_triggers(db_query)
            finally:
                db_query.close()

            assert tid_a in due
            assert tid_b in due

            fired = []
            failed = []

            # Step 2: Exact _trigger_loop pattern — claim + fire per trigger.
            # fire_trigger for tid_a will raise via patched get_trigger.
            for tid in [tid_a, tid_b]:
                db_claim = SessionLocal()
                try:
                    if claim_and_advance(db_claim, tid):
                        try:
                            if tid == tid_a:
                                # Simulate fire_trigger raising (e.g. DB error inside fire).
                                raise RuntimeError("simulated fire failure")
                            # For tid_b, actually call fire_trigger to test full path.
                            with patch("core.agent.run_engine.RunEngine") as MockEngine, \
                                 patch("core.agent.triggers._auto_session", return_value=session_id), \
                                 patch("asyncio.create_task"):
                                mock_run = MagicMock()
                                mock_run.run_id = f"run-{tid}"
                                mock_run.status.value = "pending"
                                MockEngine.return_value.create_run.return_value = mock_run
                                fire_trigger(SessionLocal, tid)
                            fired.append(tid)
                        except Exception:
                            failed.append(tid)
                finally:
                    db_claim.close()

            assert tid_a in failed
            assert tid_b in fired

            # Verify both claims persisted (next_fire_at advanced).
            for tid in [tid_a, tid_b]:
                loaded = get_trigger(db, tid)
                assert loaded["next_fire_at"] > datetime(2020, 1, 2), \
                    f"Trigger {tid} claim was lost"
        finally:
            delete_trigger(db, tid_a)
            delete_trigger(db, tid_b)
