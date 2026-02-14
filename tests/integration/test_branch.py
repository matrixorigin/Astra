"""Tests for Branch."""

import pymysql
import pytest
from sqlalchemy import text

from api.database import get_db_session
from core.sandbox import Branch


@pytest.fixture
def db(db_session):
    return db_session


@pytest.fixture
def branch(db_session):
    # Use current database
    result = db_session.execute(text("SELECT DATABASE()"))
    current_db = result.scalar()
    
    mgr = Branch(database=current_db, db=db_session)
    
    yield mgr
    
    # 清理
    try:
        db_session.commit()
        # 清理所有可能的测试表
        result = db_session.execute(text("SHOW TABLES LIKE 'test_t%'"))
        for row in result:
            table_name = list(row._mapping.values())[0]
            db_session.execute(text(f"DROP TABLE IF EXISTS {table_name}"))
        db_session.commit()
    except Exception:
        pass


def test_create_branch(branch, db_session):
    """Test branch creation."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    db_session.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db_session.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1),(2,2)"))
    db_session.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")

    result = db_session.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    count = result.first()._mapping["count"]
    assert count == 2
    
    # Cleanup
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t1_{suffix}"))
    db_session.commit()


def test_diff(branch, db_session):
    """Test diff."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    db_session.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db_session.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1),(2,2)"))
    db_session.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")
    branch.create(f"test_t2_{suffix}", f"test_t0_{suffix}")

    db_session.execute(text(f"INSERT INTO test_t1_{suffix} VALUES (3,3)"))
    db_session.execute(text(f"INSERT INTO test_t2_{suffix} VALUES (4,4)"))
    db_session.commit()

    diff = branch.diff(f"test_t2_{suffix}", f"test_t1_{suffix}")
    assert len(diff) > 0
    
    # Cleanup
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t1_{suffix}"))
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t2_{suffix}"))
    db_session.commit()


def test_merge(branch, db_session):
    """Test merge."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    db_session.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db_session.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1),(2,2)"))
    db_session.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")
    branch.create(f"test_t2_{suffix}", f"test_t0_{suffix}")

    db_session.execute(text(f"INSERT INTO test_t2_{suffix} VALUES (3,3)"))
    db_session.commit()

    branch.merge(f"test_t2_{suffix}", f"test_t1_{suffix}")

    result = db_session.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    count = result.first()._mapping["count"]
    assert count == 3
    
    # Cleanup
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t1_{suffix}"))
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t2_{suffix}"))
    db_session.commit()


def test_delete(branch, db_session):
    """Test delete."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    db_session.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db_session.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1)"))
    db_session.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")

    result = db_session.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    count = result.first()._mapping["count"]
    assert count == 1

    branch.delete(f"test_t1_{suffix}")

    # SQLAlchemy wraps pymysql errors
    from sqlalchemy.exc import ProgrammingError
    with pytest.raises(ProgrammingError):
        db_session.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    
    # Cleanup
    db_session.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db_session.commit()


def test_diff_with_snapshot(branch, db_session):
    """Test diff with snapshots."""
    from core.git_for_data import GitForData
    from uuid_utils import uuid7
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀

    git = GitForData(db=db_session)
    
    # Use unique snapshot names
    snap1 = f"snap_{str(uuid7()).replace('-', '_')}"
    snap2 = f"snap_{str(uuid7()).replace('-', '_')}"

    # Create table and snapshot
    db_session.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db_session.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1), (2,2)"))
    db_session.commit()
    git.create_snapshot(snap1)

    # Modify and create another snapshot
    db_session.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (3,3)"))
    db_session.commit()
    git.create_snapshot(snap2)

    # Diff between snapshots
    diff = branch.diff(f"test_t0_{suffix}", f"test_t0_{suffix}", source_snapshot=snap1, target_snapshot=snap2)
    assert len(diff) > 0

    # Cleanup
    git.drop_snapshot(snap1)
    git.drop_snapshot(snap2)
    db_session.commit()
    db_session.execute(text(f"DROP TABLE test_t0_{suffix}"))
    db_session.commit()


def test_diff_output_count(branch, db_session):
    """Test diff with count output."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    db_session.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db_session.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1), (2,2)"))
    db_session.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")
    db_session.execute(text(f"INSERT INTO test_t1_{suffix} VALUES (3,3)"))
    db_session.commit()

    # Diff with count
    result = branch.diff(f"test_t1_{suffix}", f"test_t0_{suffix}", output="count")
    assert len(result) > 0

    # Cleanup
    branch.delete(f"test_t1_{suffix}")
    db_session.commit()
    db_session.execute(text(f"DROP TABLE test_t0_{suffix}"))
    db_session.commit()
