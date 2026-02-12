"""Tests for Branch."""

import pymysql
import pytest

from core.sandbox import Branch
from sdk import Database


@pytest.fixture
def db():
    return Database()


@pytest.fixture
def branch(db):
    # Get current database name from settings
    from config.settings import get_settings
    settings = get_settings()
    mgr = Branch(database=settings.matrixone_database, db=db)
    # Cleanup
    try:
        db.execute("DROP TABLE IF EXISTS test_t0")
        db.execute("DROP TABLE IF EXISTS test_t1")
        db.execute("DROP TABLE IF EXISTS test_t2")
    except Exception:
        pass
    yield mgr
    # Cleanup
    try:
        db.execute("DROP TABLE IF EXISTS test_t0")
        db.execute("DROP TABLE IF EXISTS test_t1")
        db.execute("DROP TABLE IF EXISTS test_t2")
    except Exception:
        pass


def test_create_branch(branch, db):
    """Test branch creation."""
    db.execute("CREATE TABLE test_t0 (a INT, b INT, PRIMARY KEY(a))")
    db.execute("INSERT INTO test_t0 VALUES (1,1),(2,2)")

    branch.create("test_t1", "test_t0")

    count = db.fetchone("SELECT COUNT(*) as count FROM test_t1")["count"]
    assert count == 2


def test_diff(branch, db):
    """Test diff."""
    db.execute("CREATE TABLE test_t0 (a INT, b INT, PRIMARY KEY(a))")
    db.execute("INSERT INTO test_t0 VALUES (1,1),(2,2)")

    branch.create("test_t1", "test_t0")
    branch.create("test_t2", "test_t0")

    db.execute("INSERT INTO test_t1 VALUES (3,3)")
    db.execute("INSERT INTO test_t2 VALUES (4,4)")

    diff = branch.diff("test_t2", "test_t1")
    assert len(diff) > 0


def test_merge(branch, db):
    """Test merge."""
    db.execute("CREATE TABLE test_t0 (a INT, b INT, PRIMARY KEY(a))")
    db.execute("INSERT INTO test_t0 VALUES (1,1),(2,2)")

    branch.create("test_t1", "test_t0")
    branch.create("test_t2", "test_t0")

    db.execute("INSERT INTO test_t2 VALUES (3,3)")

    branch.merge("test_t2", "test_t1")

    count = db.fetchone("SELECT COUNT(*) as count FROM test_t1")["count"]
    assert count == 3


def test_delete(branch, db):
    """Test delete."""
    db.execute("CREATE TABLE test_t0 (a INT, b INT, PRIMARY KEY(a))")
    db.execute("INSERT INTO test_t0 VALUES (1,1)")

    branch.create("test_t1", "test_t0")

    count = db.fetchone("SELECT COUNT(*) as count FROM test_t1")["count"]
    assert count == 1

    branch.delete("test_t1")

    with pytest.raises(pymysql.err.ProgrammingError):
        db.fetchone("SELECT COUNT(*) as count FROM test_t1")


def test_diff_with_snapshot(branch, db):
    """Test diff with snapshots."""
    from sdk.git_for_data import GitForData

    git = GitForData(db=db)

    # Create table and snapshot
    db.execute("CREATE TABLE test_t0 (a INT, b INT, PRIMARY KEY(a))")
    db.execute("INSERT INTO test_t0 VALUES (1,1), (2,2)")
    git.create_snapshot("snap1")

    # Modify and create another snapshot
    db.execute("INSERT INTO test_t0 VALUES (3,3)")
    git.create_snapshot("snap2")

    # Diff between snapshots
    diff = branch.diff("test_t0", "test_t0", source_snapshot="snap1", target_snapshot="snap2")
    assert len(diff) > 0

    # Cleanup
    git.drop_snapshot("snap1")
    git.drop_snapshot("snap2")
    db.execute("DROP TABLE test_t0")


def test_diff_output_count(branch, db):
    """Test diff with count output."""
    db.execute("CREATE TABLE test_t0 (a INT, b INT, PRIMARY KEY(a))")
    db.execute("INSERT INTO test_t0 VALUES (1,1), (2,2)")

    branch.create("test_t1", "test_t0")
    db.execute("INSERT INTO test_t1 VALUES (3,3)")

    # Diff with count
    result = branch.diff("test_t1", "test_t0", output="count")
    assert len(result) > 0

    # Cleanup
    branch.delete("test_t1")
    db.execute("DROP TABLE test_t0")
