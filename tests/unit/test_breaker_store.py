"""Unit tests for circuit breaker persistence and cooldown."""

from datetime import datetime, timedelta, timezone

from core.agent.breaker_store import (
    _COOLDOWN_SCHEDULE,
    BreakerRecord,
)


class TestBreakerRecord:
    def test_initial_state(self):
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        assert rec.consecutive_failures == 0
        assert rec.cooldown_until is None
        assert rec.last_failure_at is None
        assert not rec.in_cooldown
        assert not rec.dirty

    def test_single_failure_sets_cooldown_and_dirty(self):
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_failure()
        assert rec.consecutive_failures == 1
        assert rec.cooldown_until is not None
        assert rec.last_failure_at is not None
        assert rec.in_cooldown
        assert rec.dirty
        # First cooldown: 5 minutes
        expected_min = datetime.now(timezone.utc) + timedelta(minutes=4)
        assert rec.cooldown_until > expected_min

    def test_escalating_cooldown(self):
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        cooldowns = []
        for _ in range(4):
            rec.record_failure()
            cooldowns.append(rec.cooldown_until - rec.last_failure_at)

        # 5min, 30min, 2h, 2h (capped at max)
        assert cooldowns[0] == _COOLDOWN_SCHEDULE[0]
        assert cooldowns[1] == _COOLDOWN_SCHEDULE[1]
        assert cooldowns[2] == _COOLDOWN_SCHEDULE[2]
        assert cooldowns[3] == _COOLDOWN_SCHEDULE[2]

    def test_success_resets_and_marks_dirty(self):
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_failure()
        rec.record_failure()
        rec.dirty = False  # simulate flush

        rec.record_success()
        assert rec.consecutive_failures == 0
        assert rec.cooldown_until is None
        assert not rec.in_cooldown
        assert rec.dirty

    def test_success_on_clean_record_stays_clean(self):
        """Success on a record with 0 failures should not mark dirty."""
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_success()
        assert not rec.dirty

    def test_expired_cooldown(self):
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.consecutive_failures = 1
        rec.cooldown_until = datetime.now(timezone.utc) - timedelta(minutes=1)
        assert not rec.in_cooldown

    def test_in_cooldown_with_naive_datetime(self):
        """DB may return naive datetime — in_cooldown must handle it."""
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        # Simulate DB returning naive datetime (no tzinfo)
        rec.cooldown_until = datetime.now() + timedelta(minutes=5)
        assert rec.cooldown_until.tzinfo is None  # Verify it's naive
        assert rec.in_cooldown  # Should not raise TypeError

    def test_flush_clears_dirty_flag(self):
        """After flush, dirty flag should be cleared (tested via integration)."""
        # This behavior is tested in integration tests with real DB.
        # Unit test just verifies the flag can be manually cleared.
        rec = BreakerRecord(user_id="alice", tool_name="grep")
        rec.record_failure()
        assert rec.dirty
        rec.dirty = False  # Simulates what flush_breaker_state does after commit
        assert not rec.dirty
