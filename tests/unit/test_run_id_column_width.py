"""Verify run_id columns are String(36) and idx_events_run_type index exists.

Two layers:
- ORM model metadata (unit, no DB needed)
- Real DB schema via SHOW COLUMNS / SHOW INDEX (integration, ground truth)
"""

import pytest
from sqlalchemy import String, text

from api.models.agent import Event, RunEvent
from api.models.workflow import WorkflowRun


# ---------------------------------------------------------------------------
# Unit: ORM model metadata
# ---------------------------------------------------------------------------

class TestRunIdColumnWidth:
    """run_id stores UUIDs — must be String(36), not String(255)."""

    @pytest.mark.parametrize("model,column", [
        (Event, "run_id"),
        (Event, "parent_run_id"),
        (RunEvent, "run_id"),
        (RunEvent, "event_id"),
        (WorkflowRun, "agent_run_id"),
    ])
    def test_column_is_string_36(self, model, column):
        col = model.__table__.c[column]
        assert isinstance(col.type, String) and col.type.length == 36


class TestCancelledQueryIndex:
    """_is_cancelled_in_db queries (run_id, event_type) — must have composite index."""

    def test_idx_events_run_type_exists(self):
        idx = next(
            (i for i in Event.__table__.indexes if i.name == "idx_events_run_type"),
            None,
        )
        assert idx is not None
        assert [c.name for c in idx.columns] == ["run_id", "event_type"]


# ---------------------------------------------------------------------------
# Integration: real DB ground truth
# ---------------------------------------------------------------------------

@pytest.fixture
def db(db_session):
    yield db_session


class TestRunIdColumnWidthDB:
    """Verify actual DB column type matches ORM declaration."""

    @pytest.mark.parametrize("table,column", [
        ("agent_events", "run_id"),
        ("agent_events", "parent_run_id"),
        ("agent_run_events", "run_id"),
        ("agent_run_events", "event_id"),
        ("wf_runs", "agent_run_id"),
    ])
    def test_db_column_varchar_36(self, db, table, column):
        rows = db.execute(
            text(f"SHOW COLUMNS FROM {table} LIKE :col"),
            {"col": column},
        ).fetchall()
        assert len(rows) == 1
        col_type = rows[0][1].upper()
        assert "VARCHAR(36)" in col_type, f"{table}.{column} is {col_type}, expected VARCHAR(36)"


class TestCancelledQueryIndexDB:
    """Verify idx_events_run_type exists in real DB."""

    def test_index_exists_in_db(self, db):
        rows = db.execute(text("SHOW INDEX FROM agent_events")).fetchall()
        idx_rows = [r for r in rows if r[2] == "idx_events_run_type"]
        assert len(idx_rows) == 2, "Expected 2-column composite index"
        cols = {r[3]: r[4] for r in idx_rows}  # seq_in_index -> column_name
        assert cols[1] == "run_id"
        assert cols[2] == "event_type"


class TestRunIdValueRoundtrip:
    """Write a UUID run_id, read it back — verify no truncation or corruption."""

    def test_event_run_id_persists_uuid(self, db):
        from uuid_utils import uuid7

        eid = str(uuid7())
        rid = str(uuid7())
        chain = str(uuid7())
        sid = str(uuid7())

        db.execute(text(
            "INSERT INTO agent_events "
            "(event_id, session_id, user_id, agent_id, agent_version, "
            " event_type, content, causal_chain_id, run_id, created_at) "
            "VALUES (:eid, :sid, 'test', 'system', '1.0.0', "
            " 'test', 'x', :chain, :rid, NOW())"
        ), {"eid": eid, "sid": sid, "chain": chain, "rid": rid})
        db.commit()

        row = db.execute(
            text("SELECT run_id FROM agent_events WHERE event_id = :eid"),
            {"eid": eid},
        ).fetchone()
        assert row is not None
        assert row[0] == rid

        # cleanup
        db.execute(text("DELETE FROM agent_events WHERE event_id = :eid"), {"eid": eid})
        db.commit()
