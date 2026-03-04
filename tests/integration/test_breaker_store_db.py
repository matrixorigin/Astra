"""Integration tests for circuit breaker persistence with real DB.

Verifies:
- Table creation
- load/flush round-trip with field-level verification
- Cooldown persistence across simulated turns
- Dirty tracking prevents unnecessary writes
"""

from datetime import datetime, timedelta, timezone

import pytest
from sqlalchemy import text

from core.agent.breaker_store import (
    BreakerRecord,
    ToolBreakerState,
    flush_breaker_state,
    load_breaker_state,
)


@pytest.fixture(autouse=True)
def clean_breaker_table(db):
    """Ensure table exists and is empty before each test."""
    ToolBreakerState.__table__.create(db.get_bind(), checkfirst=True)
    db.execute(text("DELETE FROM tool_breaker_state"))
    db.commit()
    yield
    db.execute(text("DELETE FROM tool_breaker_state"))
    db.commit()


class TestBreakerStoreIntegration:
    """Real DB tests for breaker persistence."""

    def test_flush_and_load_round_trip(self, db):
        """Flush a record, load it back, verify every field."""
        before = datetime.now(timezone.utc).replace(microsecond=0)

        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_failure()
        rec.record_failure()  # 2 failures → 30min cooldown

        records = {"grep": rec}
        written = flush_breaker_state(db, records)
        assert written == 1
        assert not rec.dirty  # dirty cleared after flush

        after = datetime.now(timezone.utc).replace(microsecond=0) + timedelta(seconds=1)

        # Re-query from DB (not from return value)
        loaded = load_breaker_state(db, "alice")
        assert "grep" in loaded
        saved = loaded["grep"]

        # Verify EVERY field
        assert saved.user_id == "alice"
        assert saved.tool_name == "grep"
        assert saved.consecutive_failures == 2
        assert saved.last_failure_at is not None
        # DB returns naive datetime (second precision) — normalize for comparison
        lfa = saved.last_failure_at.replace(tzinfo=timezone.utc) if saved.last_failure_at.tzinfo is None else saved.last_failure_at
        assert before <= lfa <= after
        assert saved.cooldown_until is not None
        cu = saved.cooldown_until.replace(tzinfo=timezone.utc) if saved.cooldown_until.tzinfo is None else saved.cooldown_until
        assert cu > lfa
        # 2 failures → 30min cooldown
        assert timedelta(minutes=29) <= (cu - lfa) <= timedelta(minutes=31)
        assert not saved.dirty  # loaded records start clean

    def test_flush_updates_existing_record(self, db):
        """Flush twice — second flush updates, not inserts."""
        rec = BreakerRecord(user_id="alice", tool_name="shell")
        rec.record_failure()
        flush_breaker_state(db, {"shell": rec})

        # Verify 1 failure
        loaded = load_breaker_state(db, "alice")
        assert loaded["shell"].consecutive_failures == 1

        # Second failure
        rec.record_failure()
        flush_breaker_state(db, {"shell": rec})

        loaded = load_breaker_state(db, "alice")
        assert loaded["shell"].consecutive_failures == 2

        # Verify only 1 row (not 2)
        count = db.query(ToolBreakerState).filter_by(
            user_id="alice", tool_name="shell",
        ).count()
        assert count == 1

    def test_success_resets_in_db(self, db):
        """After success, all failure state is cleared in DB."""
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_failure()
        rec.record_failure()
        flush_breaker_state(db, {"grep": rec})

        # Verify failures persisted
        loaded = load_breaker_state(db, "alice")
        assert loaded["grep"].consecutive_failures == 2
        assert loaded["grep"].cooldown_until is not None
        assert loaded["grep"].last_failure_at is not None

        # Success
        rec.record_success()
        flush_breaker_state(db, {"grep": rec})

        loaded = load_breaker_state(db, "alice")
        assert loaded["grep"].consecutive_failures == 0
        assert loaded["grep"].cooldown_until is None
        assert loaded["grep"].last_failure_at is None  # Also cleared

    def test_dirty_tracking_prevents_unnecessary_writes(self, db):
        """Clean records should not be written."""
        rec = BreakerRecord(user_id="alice", tool_name="grep", dirty=False)
        written = flush_breaker_state(db, {"grep": rec})
        assert written == 0

        # Verify nothing in DB
        loaded = load_breaker_state(db, "alice")
        assert len(loaded) == 0

    def test_multiple_tools_per_user(self, db):
        """Multiple tools for same user — each independent."""
        records = {
            "grep": BreakerRecord(user_id="alice", tool_name="grep"),
            "shell": BreakerRecord(user_id="alice", tool_name="shell"),
        }
        records["grep"].record_failure()
        records["grep"].record_failure()
        records["shell"].record_failure()

        flush_breaker_state(db, records)

        loaded = load_breaker_state(db, "alice")
        assert len(loaded) == 2
        assert loaded["grep"].consecutive_failures == 2
        assert loaded["shell"].consecutive_failures == 1

    def test_user_isolation(self, db):
        """Different users don't see each other's breaker state."""
        rec_a = BreakerRecord(user_id="alice", tool_name="grep")
        rec_a.record_failure()
        rec_b = BreakerRecord(user_id="bob", tool_name="grep")
        rec_b.record_failure()
        rec_b.record_failure()

        flush_breaker_state(db, {"grep": rec_a})
        flush_breaker_state(db, {"grep": rec_b})

        loaded_a = load_breaker_state(db, "alice")
        loaded_b = load_breaker_state(db, "bob")

        assert loaded_a["grep"].consecutive_failures == 1
        assert loaded_b["grep"].consecutive_failures == 2

    def test_load_empty_user(self, db):
        """Loading for a user with no records returns empty dict."""
        loaded = load_breaker_state(db, "nonexistent")
        assert loaded == {}

    def test_cooldown_persists_across_simulated_turns(self, db):
        """Simulate: turn 1 trips breaker → turn 2 loads cooldown."""
        # Turn 1: tool fails, breaker trips
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_failure()
        rec.record_failure()
        rec.record_failure()  # 3 failures → 2h cooldown
        flush_breaker_state(db, {"grep": rec})

        # Turn 2: load state — should see cooldown
        loaded = load_breaker_state(db, "alice")
        assert loaded["grep"].in_cooldown
        assert loaded["grep"].consecutive_failures == 3

    def test_cross_turn_failure_accumulation(self, db):
        """Regression: failures from turn 1 must accumulate into turn 2.

        Scenario: tool fails 2x in turn 1 (not enough to trip breaker at 3).
        Turn 2 loads persisted state, tool fails 1x more → total 3 → breaker trips.
        This verifies that every failure is persisted, not just breaker-tripping ones.
        """
        # Turn 1: 2 failures (below the 3-failure threshold)
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_failure()
        rec.record_failure()
        assert rec.consecutive_failures == 2
        flush_breaker_state(db, {"grep": rec})

        # Turn 2: load persisted state
        loaded = load_breaker_state(db, "alice")
        assert loaded["grep"].consecutive_failures == 2
        assert not loaded["grep"].dirty  # freshly loaded

        # Turn 2: 1 more failure → total 3
        loaded["grep"].record_failure()
        assert loaded["grep"].consecutive_failures == 3
        assert loaded["grep"].dirty

        # Verify cooldown escalated correctly: 3rd failure → 2h cooldown
        assert loaded["grep"].cooldown_until is not None
        expected_cooldown = timedelta(hours=2)
        actual_cooldown = loaded["grep"].cooldown_until - loaded["grep"].last_failure_at
        # Allow 1 second tolerance for test execution time
        assert abs(actual_cooldown - expected_cooldown) < timedelta(seconds=1)

        # Persist and reload to verify round-trip
        flush_breaker_state(db, loaded)
        reloaded = load_breaker_state(db, "alice")
        assert reloaded["grep"].consecutive_failures == 3
        assert reloaded["grep"].in_cooldown
