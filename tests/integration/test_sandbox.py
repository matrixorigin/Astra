"""Tests for Sandbox (branch-based implementation)."""

import pytest
from uuid_utils import uuid7
from sqlalchemy import text

from core.sandbox import Sandbox


SOURCE_DB = "test_dev_agent_v3"


@pytest.fixture
def sandbox(db_session):
    return Sandbox(db=db_session, source_db=SOURCE_DB)


@pytest.fixture
def source_table(db_session):
    """Create a test table in source DB for branching."""
    db_session.execute(text(f"CREATE TABLE IF NOT EXISTS {SOURCE_DB}.test_branch_src (id INT PRIMARY KEY, val INT)"))
    db_session.execute(text(f"DELETE FROM {SOURCE_DB}.test_branch_src"))
    db_session.execute(text(f"INSERT INTO {SOURCE_DB}.test_branch_src VALUES (1,10),(2,20),(3,30)"))
    db_session.commit()
    yield "test_branch_src"
    db_session.execute(text(f"DROP TABLE IF EXISTS {SOURCE_DB}.test_branch_src"))
    db_session.commit()


def _unique_name():
    return f"sandbox_{str(uuid7()).replace('-', '_')[:16]}".lower()


# ===========================================================================
# Lifecycle
# ===========================================================================

def test_create_and_delete(sandbox):
    name = _unique_name()
    sandbox.create(name, description="Test sandbox")
    sandboxes = sandbox.list_sandboxes()
    assert any(s["sandbox_name"] == name for s in sandboxes)

    sandbox.delete(name)
    sandboxes = sandbox.list_sandboxes()
    assert not any(s["sandbox_name"] == name for s in sandboxes)


def test_create_with_tables(sandbox, db_session, source_table):
    """Create sandbox with table branching (zero-copy)."""
    name = _unique_name()
    sandbox.create(name, tables=[source_table])

    r = db_session.execute(text(f"SELECT count(*) FROM {name}.{source_table}"))
    assert r.scalar() == 3

    sandbox.delete(name)


def test_list_with_filter(sandbox):
    sandbox.create("sandbox_exp_test1")
    sandbox.create("sandbox_prod_test2")

    exp = sandbox.list_sandboxes(pattern="%exp%")
    assert any("exp" in s["sandbox_name"] for s in exp)

    sandbox.delete("sandbox_exp_test1")
    sandbox.delete("sandbox_prod_test2")


# ===========================================================================
# Table management
# ===========================================================================

def test_add_remove_table(sandbox, db_session, source_table):
    name = _unique_name()
    sandbox.create(name)

    sandbox.add_table(name, source_table)
    assert source_table in sandbox.list_tables(name)

    r = db_session.execute(text(f"SELECT count(*) FROM {name}.{source_table}"))
    assert r.scalar() == 3

    sandbox.remove_table(name, source_table)
    assert source_table not in sandbox.list_tables(name)

    sandbox.delete(name)


def test_isolation(sandbox, db_session, source_table):
    """Sandbox modifications don't affect source."""
    name = _unique_name()
    sandbox.create(name, tables=[source_table])

    # Modify in sandbox
    db_session.execute(text(f"INSERT INTO {name}.{source_table} VALUES (4, 40)"))
    db_session.commit()

    # Source unchanged
    r = db_session.execute(text(f"SELECT count(*) FROM {SOURCE_DB}.{source_table}"))
    assert r.scalar() == 3

    # Sandbox has 4
    r = db_session.execute(text(f"SELECT count(*) FROM {name}.{source_table}"))
    assert r.scalar() == 4

    sandbox.delete(name)


# ===========================================================================
# Snapshot & Restore (on sandbox database)
# ===========================================================================

def test_snapshot_and_restore(sandbox, db_session, source_table):
    name = _unique_name()
    sandbox.create(name, tables=[source_table])

    # Snapshot
    sandbox.snapshot(name, "snap1")
    snapshots = sandbox.list_snapshots(name)
    assert any(s["name"] == "snap1" for s in snapshots)

    # Modify sandbox
    db_session.execute(text(f"DELETE FROM {name}.{source_table}"))
    db_session.commit()
    r = db_session.execute(text(f"SELECT count(*) FROM {name}.{source_table}"))
    assert r.scalar() == 0

    # Restore
    sandbox.restore(name, "snap1")
    r = db_session.execute(text(f"SELECT count(*) FROM {name}.{source_table}"))
    assert r.scalar() == 3

    sandbox.delete(name)


def test_delete_cleans_snapshots(sandbox, db_session, source_table):
    name = _unique_name()
    sandbox.create(name, tables=[source_table])
    sandbox.snapshot(name, "s1")
    sandbox.snapshot(name, "s2")

    sandbox.delete(name)

    # Snapshots should be gone
    assert sandbox.list_snapshots(name) == []


# ===========================================================================
# Diff & Merge
# ===========================================================================

def test_diff(sandbox, db_session, source_table):
    name = _unique_name()
    sandbox.create(name, tables=[source_table])

    # No changes → empty diff
    assert sandbox.diff(name, [source_table]) == []

    # Insert in sandbox
    db_session.execute(text(f"INSERT INTO {name}.{source_table} VALUES (4, 40)"))
    db_session.commit()

    diffs = sandbox.diff(name, [source_table])
    assert len(diffs) == 1
    assert diffs[0]["table"] == source_table
    assert any(r["flag"] == "INSERT" for r in diffs[0]["rows"])

    sandbox.delete(name)


def test_merge(sandbox, db_session, source_table):
    name = _unique_name()
    sandbox.create(name, tables=[source_table])

    # Modify sandbox
    db_session.execute(text(f"INSERT INTO {name}.{source_table} VALUES (5, 50)"))
    db_session.commit()

    # Merge back
    result = sandbox.merge(name, [source_table], on_conflict="accept")
    assert result["merged"] == [source_table]
    assert result["failed"] == []

    # Source now has the new row
    r = db_session.execute(text(f"SELECT val FROM {SOURCE_DB}.{source_table} WHERE id=5"))
    assert r.scalar() == 50

    sandbox.delete(name)


# ===========================================================================
# Info & Use
# ===========================================================================

def test_info(sandbox, db_session, source_table):
    name = _unique_name()
    sandbox.create(name, description="Test info", tables=[source_table])

    info = sandbox.info(name)
    assert info["sandbox_name"] == name
    assert info["table_count"] >= 1

    sandbox.delete(name)


def test_use_sandbox(sandbox, db_session, source_table):
    name = _unique_name()
    sandbox.create(name, tables=[source_table])

    # Get current DB
    current_db = db_session.execute(text("SELECT DATABASE()")).scalar()

    # Switch to sandbox
    sandbox.use(name)
    r = db_session.execute(text(f"SELECT count(*) FROM {source_table}"))
    assert r.scalar() == 3

    # Switch back
    sandbox.use(current_db)
    sandbox.delete(name)
