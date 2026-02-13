"""Tests for Branch."""

import pymysql
import pytest
from sqlalchemy import text

from api.database import get_db_session
from core.sandbox import Branch


@pytest.fixture
def db():
    db_session = next(get_db_session())
    # 确保在正确的数据库中
    from config.settings import get_settings
    settings = get_settings()
    db_session.execute(text(f"USE {settings.matrixone_database}"))
    db_session.commit()
    return db_session


@pytest.fixture
def branch():
    # 每个测试都使用新的 session，避免冲突
    from config.settings import get_settings
    settings = get_settings()
    
    db = next(get_db_session())
    mgr = Branch(database=settings.matrixone_database, db=db)
    
    yield mgr
    
    # 清理
    try:
        db.commit()
        # 清理所有可能的测试表
        result = db.execute(text("SHOW TABLES LIKE 'test_t%'"))
        for row in result:
            table_name = list(row._mapping.values())[0]
            db.execute(text(f"DROP TABLE IF EXISTS {table_name}"))
        db.commit()
        db.close()
    except Exception:
        pass


def test_create_branch(branch, db):
    """Test branch creation."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    # 确保在正确的数据库中
    from config.settings import get_settings
    settings = get_settings()
    db.execute(text(f"USE {settings.matrixone_database}"))
    db.commit()
    
    db.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1),(2,2)"))
    db.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")

    result = db.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    count = result.first()._mapping["count"]
    assert count == 2
    
    # Cleanup
    db.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db.execute(text(f"DROP TABLE IF EXISTS test_t1_{suffix}"))
    db.commit()


def test_diff(branch, db):
    """Test diff."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    # 确保在正确的数据库中
    from config.settings import get_settings
    settings = get_settings()
    db.execute(text(f"USE {settings.matrixone_database}"))
    db.commit()
    
    db.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1),(2,2)"))
    db.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")
    branch.create(f"test_t2_{suffix}", f"test_t0_{suffix}")

    db.execute(text(f"INSERT INTO test_t1_{suffix} VALUES (3,3)"))
    db.execute(text(f"INSERT INTO test_t2_{suffix} VALUES (4,4)"))
    db.commit()

    diff = branch.diff(f"test_t2_{suffix}", f"test_t1_{suffix}")
    assert len(diff) > 0
    
    # Cleanup
    db.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db.execute(text(f"DROP TABLE IF EXISTS test_t1_{suffix}"))
    db.execute(text(f"DROP TABLE IF EXISTS test_t2_{suffix}"))
    db.commit()


def test_merge(branch, db):
    """Test merge."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    # 确保在正确的数据库中
    from config.settings import get_settings
    settings = get_settings()
    db.execute(text(f"USE {settings.matrixone_database}"))
    db.commit()
    
    db.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1),(2,2)"))
    db.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")
    branch.create(f"test_t2_{suffix}", f"test_t0_{suffix}")

    db.execute(text(f"INSERT INTO test_t2_{suffix} VALUES (3,3)"))
    db.commit()

    branch.merge(f"test_t2_{suffix}", f"test_t1_{suffix}")

    result = db.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    count = result.first()._mapping["count"]
    assert count == 3
    
    # Cleanup
    db.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db.execute(text(f"DROP TABLE IF EXISTS test_t1_{suffix}"))
    db.execute(text(f"DROP TABLE IF EXISTS test_t2_{suffix}"))
    db.commit()


def test_delete(branch, db):
    """Test delete."""
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀
    
    # 确保在正确的数据库中
    from config.settings import get_settings
    settings = get_settings()
    db.execute(text(f"USE {settings.matrixone_database}"))
    db.commit()
    
    db.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1)"))
    db.commit()

    branch.create(f"test_t1_{suffix}", f"test_t0_{suffix}")

    result = db.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    count = result.first()._mapping["count"]
    assert count == 1

    branch.delete(f"test_t1_{suffix}")

    # SQLAlchemy wraps pymysql errors
    from sqlalchemy.exc import ProgrammingError
    with pytest.raises(ProgrammingError):
        db.execute(text(f"SELECT COUNT(*) as count FROM test_t1_{suffix}"))
    
    # Cleanup
    db.execute(text(f"DROP TABLE IF EXISTS test_t0_{suffix}"))
    db.commit()


def test_diff_with_snapshot(branch, db):
    """Test diff with snapshots."""
    from core.git_for_data import GitForData
    from uuid_utils import uuid7
    import time
    suffix = str(int(time.time() * 1000) % 10000)  # 唯一后缀

    # 确保在正确的数据库中
    from config.settings import get_settings
    settings = get_settings()
    db.execute(text(f"USE {settings.matrixone_database}"))
    db.commit()

    git = GitForData(db=db)
    
    # Use unique snapshot names
    snap1 = f"snap_{str(uuid7()).replace('-', '_')}"
    snap2 = f"snap_{str(uuid7()).replace('-', '_')}"

    # Create table and snapshot
    db.execute(text(f"CREATE TABLE test_t0_{suffix} (a INT, b INT, PRIMARY KEY(a))"))
    db.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (1,1), (2,2)"))
    db.commit()
    git.create_snapshot(snap1)

    # Modify and create another snapshot
    db.execute(text(f"INSERT INTO test_t0_{suffix} VALUES (3,3)"))
    db.commit()
    git.create_snapshot(snap2)

    # Diff between snapshots
    diff = branch.diff(f"test_t0_{suffix}", f"test_t0_{suffix}", source_snapshot=snap1, target_snapshot=snap2)
    assert len(diff) > 0

    # Cleanup
    git.drop_snapshot(snap1)
    git.drop_snapshot(snap2)
    db.commit()
    db.execute(text(f"DROP TABLE test_t0_{suffix}"))
    db.commit()


def test_diff_output_count(branch, db):
    """Test diff with count output."""
    db.commit()
    db.execute(text("CREATE TABLE test_t0 (a INT, b INT, PRIMARY KEY(a))"))
    db.execute(text("INSERT INTO test_t0 VALUES (1,1), (2,2)"))
    db.commit()

    branch.create("test_t1", "test_t0")
    db.execute(text("INSERT INTO test_t1 VALUES (3,3)"))
    db.commit()

    # Diff with count
    result = branch.diff("test_t1", "test_t0", output="count")
    assert len(result) > 0

    # Cleanup
    branch.delete("test_t1")
    db.commit()
    db.execute(text("DROP TABLE test_t0"))
    db.commit()
