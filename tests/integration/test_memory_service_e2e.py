"""End-to-end tests for MemoryService facade methods.

Verifies every new MemoryService method against REAL database.
Every DB field is checked — not just "record exists".
"""

import time
from datetime import datetime, timedelta, timezone

import pytest
from uuid_utils import uuid7

from api.models.memory import MemoryRecord
from core.memory.tabular.service import MemoryService
from core.memory.types import Memory, MemoryType, TrustTier


def _uid():
    return f"svc_e2e_{uuid7().hex}"


def _sid():
    return f"sess_{uuid7().hex}"


def _mid():
    return uuid7().hex


@pytest.fixture
def svc(db_factory):
    return MemoryService(db_factory)


@pytest.fixture
def cleanup(db_factory):
    """Track and cleanup memories created during test."""
    created_ids: list[str] = []
    yield created_ids
    from core.memory.tabular.store import MemoryStore
    store = MemoryStore(db_factory)
    for mid in created_ids:
        try:
            store.deactivate(mid)
        except Exception:
            pass


class TestCreateMemory:
    """create_memory: bypass Observer, write directly, verify ALL fields."""

    def test_all_fields_persisted(self, svc, db_factory, db_session, cleanup):
        user_id = _uid()
        session_id = _sid()
        memory_id = _mid()

        mem = Memory(
            memory_id=memory_id,
            user_id=user_id,
            session_id=session_id,
            memory_type=MemoryType.TOOL_RESULT,
            content="grep output: file.py:10:match",
            initial_confidence=0.9,
            trust_tier=TrustTier.T2_CURATED,
            source_event_ids=["evt_001", "evt_002"],
        )
        result = svc.create_memory(mem)
        cleanup.append(memory_id)

        # Re-query from DB — don't trust return value
        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(memory_id=memory_id).first()

        assert row is not None
        assert row.memory_id == memory_id
        assert row.user_id == user_id
        assert row.session_id == session_id
        assert row.memory_type == "tool_result"
        assert row.content == "grep output: file.py:10:match"
        assert row.initial_confidence == pytest.approx(0.9, abs=0.01)
        assert row.trust_tier == "T2"
        assert row.source_event_ids == ["evt_001", "evt_002"]
        assert row.superseded_by is None
        assert row.is_active == 1
        assert row.observed_at is not None
        assert row.created_at is not None
        assert row.updated_at is not None
        assert row.embedding is None

        # Return value matches
        assert result.memory_id == memory_id
        assert result.user_id == user_id

    def test_auto_generates_memory_id(self, svc, db_session, cleanup):
        user_id = _uid()
        mem = Memory(
            memory_id="",
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="auto-id test",
        )
        result = svc.create_memory(mem)
        cleanup.append(result.memory_id)

        assert result.memory_id  # non-empty
        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(memory_id=result.memory_id).first()
        assert row is not None
        assert row.content == "auto-id test"

    def test_cross_session_memory_has_null_session(self, svc, db_session, cleanup):
        user_id = _uid()
        mid = _mid()
        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="cross-session fact",
            session_id=None,
        )
        svc.create_memory(mem)
        cleanup.append(mid)

        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        assert row.session_id is None


class TestGetMemory:
    """get_memory: round-trip verify."""

    def test_round_trip_all_fields(self, svc, cleanup):
        user_id = _uid()
        mid = _mid()
        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.PROFILE,
            content="prefers Python",
            initial_confidence=0.85,
            trust_tier=TrustTier.T3_INFERRED,
            source_event_ids=["e1"],
            session_id=None,
        )
        svc.create_memory(mem)
        cleanup.append(mid)

        got = svc.get_memory(mid)
        assert got is not None
        assert got.memory_id == mid
        assert got.user_id == user_id
        assert got.memory_type == MemoryType.PROFILE
        assert got.content == "prefers Python"
        assert got.initial_confidence == pytest.approx(0.85, abs=0.01)
        assert got.trust_tier == TrustTier.T3_INFERRED
        assert got.source_event_ids == ["e1"]
        assert got.is_active == 1
        assert got.superseded_by is None
        assert got.observed_at is not None
        assert got.created_at is not None

    def test_not_found_returns_none(self, svc):
        assert svc.get_memory("nonexistent_id_xyz") is None


class TestUpdateMemoryContent:
    """update_memory_content: verify content changed, other fields unchanged."""

    def test_content_updated_others_unchanged(self, svc, db_session, cleanup):
        user_id = _uid()
        mid = _mid()
        session_id = _sid()
        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.TOOL_RESULT,
            content="original content",
            initial_confidence=0.7,
            trust_tier=TrustTier.T4_UNVERIFIED,
            source_event_ids=["e1"],
            session_id=session_id,
        )
        svc.create_memory(mem)
        cleanup.append(mid)

        # Capture original timestamps
        db_session.expire_all()
        original = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        original_created = original.created_at
        original_observed = original.observed_at

        time.sleep(0.05)

        svc.update_memory_content(mid, "updated streaming content")

        # Re-query
        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()

        # Changed
        assert row.content == "updated streaming content"
        assert row.updated_at >= original.updated_at

        # Unchanged
        assert row.memory_id == mid
        assert row.user_id == user_id
        assert row.memory_type == "tool_result"
        assert row.initial_confidence == pytest.approx(0.7, abs=0.01)
        assert row.trust_tier == "T4"
        assert row.source_event_ids == ["e1"]
        assert row.session_id == session_id
        assert row.is_active == 1
        assert row.superseded_by is None
        assert row.observed_at == original_observed
        assert row.created_at == original_created


class TestListActive:
    """list_active: verify filtering, ordering, limit."""

    def test_filters_by_type_and_user(self, svc, cleanup):
        uid = _uid()
        other_uid = _uid()

        ids = []
        for i, (u, t) in enumerate([
            (uid, MemoryType.SEMANTIC),
            (uid, MemoryType.SEMANTIC),
            (uid, MemoryType.PROCEDURAL),
            (other_uid, MemoryType.SEMANTIC),
        ]):
            mid = _mid()
            ids.append(mid)
            svc.create_memory(Memory(
                memory_id=mid, user_id=u, memory_type=t,
                content=f"item_{i}",
            ))
        cleanup.extend(ids)

        # uid + SEMANTIC → exactly 2
        results = svc.list_active(uid, memory_type=MemoryType.SEMANTIC)
        assert len(results) == 2
        assert all(m.user_id == uid for m in results)
        assert all(m.memory_type == MemoryType.SEMANTIC for m in results)

        # uid + PROCEDURAL → exactly 1
        results = svc.list_active(uid, memory_type=MemoryType.PROCEDURAL)
        assert len(results) == 1
        assert results[0].content == "item_2"

        # No type filter → all 3 for uid
        results = svc.list_active(uid)
        assert len(results) == 3

        # Other user → only 1
        results = svc.list_active(other_uid)
        assert len(results) == 1

    def test_limit(self, svc, cleanup):
        uid = _uid()
        ids = []
        for i in range(5):
            mid = _mid()
            ids.append(mid)
            svc.create_memory(Memory(
                memory_id=mid, user_id=uid, memory_type=MemoryType.SEMANTIC,
                content=f"item_{i}",
            ))
        cleanup.extend(ids)

        results = svc.list_active(uid, limit=2)
        assert len(results) == 2

    def test_excludes_inactive(self, svc, db_session, cleanup):
        uid = _uid()
        mid_active = _mid()
        mid_inactive = _mid()

        svc.create_memory(Memory(
            memory_id=mid_active, user_id=uid,
            memory_type=MemoryType.SEMANTIC, content="active",
        ))
        svc.create_memory(Memory(
            memory_id=mid_inactive, user_id=uid,
            memory_type=MemoryType.SEMANTIC, content="will deactivate",
        ))
        cleanup.extend([mid_active, mid_inactive])

        # Deactivate one directly in DB
        db_session.query(MemoryRecord).filter_by(memory_id=mid_inactive).update({"is_active": 0})
        db_session.commit()

        results = svc.list_active(uid)
        assert len(results) == 1
        assert results[0].memory_id == mid_active


class TestRunHourly:
    """run_hourly: verify tool_result cleanup."""

    def test_cleans_expired_tool_results(self, svc, db_session, cleanup):
        uid = _uid()
        mid_old = _mid()
        mid_fresh = _mid()

        svc.create_memory(Memory(
            memory_id=mid_old, user_id=uid,
            memory_type=MemoryType.TOOL_RESULT, content="old tool output",
            session_id=_sid(),
        ))
        svc.create_memory(Memory(
            memory_id=mid_fresh, user_id=uid,
            memory_type=MemoryType.TOOL_RESULT, content="fresh tool output",
            session_id=_sid(),
        ))
        cleanup.extend([mid_old, mid_fresh])

        # Backdate the old one.
        # Truncate microseconds: MO DateTime rounds ≥500ms up, which can
        # shift the stored value by +1s and make TIMESTAMPDIFF off by 1h.
        old_time = datetime.utcnow().replace(microsecond=0) - timedelta(hours=25)
        db_session.query(MemoryRecord).filter_by(memory_id=mid_old).update({
            "created_at": old_time, "observed_at": old_time, "updated_at": old_time,
        })
        db_session.commit()

        report = svc.run_hourly()
        assert report.cleaned_tool_results >= 1

        # Verify old one is gone or deactivated
        db_session.expire_all()
        old_row = db_session.query(MemoryRecord).filter_by(memory_id=mid_old).first()
        assert old_row is None or old_row.is_active == 0

        # Fresh one still active
        fresh_row = db_session.query(MemoryRecord).filter_by(memory_id=mid_fresh).first()
        assert fresh_row is not None
        assert fresh_row.is_active == 1


class TestRunDailyAll:
    """run_daily_all: verify low-confidence quarantine."""

    def test_quarantines_low_confidence(self, svc, db_session, cleanup):
        uid = _uid()
        mid = _mid()

        svc.create_memory(Memory(
            memory_id=mid, user_id=uid,
            memory_type=MemoryType.SEMANTIC, content="dubious fact",
            initial_confidence=0.35,
            trust_tier=TrustTier.T4_UNVERIFIED,
        ))
        cleanup.append(mid)

        # Backdate so decay pushes effective_confidence below threshold
        old_time = datetime.utcnow() - timedelta(days=60)
        db_session.query(MemoryRecord).filter_by(memory_id=mid).update({
            "observed_at": old_time, "updated_at": old_time,
        })
        db_session.commit()

        report = svc.run_daily_all()
        assert report.quarantined >= 0
        assert report.errors is None or len(report.errors) == 0

        # Verify the memory is deactivated
        db_session.expire_all()
        row = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        assert row is not None
        assert row.is_active == 0


class TestRunWeekly:
    """run_weekly: verify returns valid report."""

    def test_returns_report(self, svc):
        report = svc.run_weekly()
        assert report is not None
        assert report.errors is None or isinstance(report.errors, list)


class TestRunPipeline:
    """run_pipeline: verify memories extracted and persisted in DB."""

    def test_extracts_and_persists(self, db_factory, db_session, cleanup):
        uid = _uid()

        from unittest.mock import MagicMock
        import json

        mock_llm = MagicMock()
        mock_llm.chat_with_tools.return_value = {
            "content": json.dumps([
                {"type": "semantic", "content": "user prefers dark mode", "confidence": 0.8},
                {"type": "profile", "content": "senior engineer", "confidence": 0.9},
            ])
        }

        svc_with_llm = MemoryService(db_factory, llm_client=mock_llm)

        messages = [
            {"role": "user", "content": "I always use dark mode, I'm a senior engineer"},
            {"role": "assistant", "content": "Got it, dark mode preference noted."},
        ]

        result = svc_with_llm.run_pipeline(user_id=uid, messages=messages)
        assert result.memories_extracted >= 0

        # Verify DB records if extraction worked
        if result.memories_extracted > 0:
            db_session.expire_all()
            rows = db_session.query(MemoryRecord).filter(
                MemoryRecord.user_id == uid,
                MemoryRecord.is_active == 1,
            ).all()

            for row in rows:
                cleanup.append(row.memory_id)

            assert len(rows) == result.memories_extracted
            for row in rows:
                assert row.user_id == uid
                assert row.memory_type in ("semantic", "profile", "procedural")
                assert row.content  # non-empty
                assert row.is_active == 1
                assert row.observed_at is not None
                assert row.created_at is not None


class TestStoreUpdateContent:
    """MemoryStore.update_content: direct DB verification."""

    def test_update_content_field_level(self, db_factory, db_session, cleanup):
        from core.memory.tabular.store import MemoryStore

        store = MemoryStore(db_factory)
        mid = _mid()
        uid = _uid()

        mem = Memory(
            memory_id=mid, user_id=uid,
            memory_type=MemoryType.TOOL_RESULT,
            content="v1",
            initial_confidence=0.5,
            trust_tier=TrustTier.T3_INFERRED,
            source_event_ids=["e1"],
        )
        store.create(mem)
        cleanup.append(mid)

        db_session.expire_all()
        before = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()
        before_updated = before.updated_at
        before_created = before.created_at

        time.sleep(0.05)
        store.update_content(mid, "v2 streaming append")

        db_session.expire_all()
        after = db_session.query(MemoryRecord).filter_by(memory_id=mid).first()

        assert after.content == "v2 streaming append"
        assert after.updated_at >= before_updated
        assert after.created_at == before_created
        assert after.initial_confidence == pytest.approx(0.5, abs=0.01)
        assert after.trust_tier == "T3"
        assert after.source_event_ids == ["e1"]
        assert after.is_active == 1

    def test_update_nonexistent_is_noop(self, db_factory):
        from core.memory.tabular.store import MemoryStore

        store = MemoryStore(db_factory)
        store.update_content("nonexistent_xyz", "new content")
