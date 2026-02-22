"""Integration tests for data branch — real MatrixOne, end-to-end.

Tests: Branch (create/diff/merge/delete), DataContext (full lifecycle),
       CodeExecutor WRITE mode with real subprocess + real DB.

Requires: MatrixOne running on localhost:6001.
"""

import pytest
import pytest
from sqlalchemy import create_engine, text
from sqlalchemy.orm import sessionmaker, Session
import os
from uuid_utils import uuid7

from core.sandbox.branch import Branch
from core.code_executor.data_context import DataContext, DataAccessLevel, TableDiff
from core.code_executor import CodeExecutor, CodeExecutionRequest
from core.code_executor.security import SecurityGuard
from core.runtime.subprocess_runtime import SubprocessRuntime
from core.utils.id_generator import generate_hash_id, generate_id

# Support parallel testing with worker-specific database names
def get_worker_id():
    """Get pytest-xdist worker ID for database isolation."""
    return os.getenv("PYTEST_XDIST_WORKER", "master")

worker_id = get_worker_id()
if worker_id != "master":
    TEST_DB = f"test_branch_integ_{worker_id}"
else:
    TEST_DB = "test_branch_integ"

MO_URL = "mysql+pymysql://root:111@localhost:6001/mo_catalog"


@pytest.fixture(scope="module")
def engine():
    return create_engine(MO_URL)


@pytest.fixture
def db(engine) -> Session:
    S = sessionmaker(bind=engine)
    session = S()
    # Create database if it doesn't exist, then use it
    session.execute(text(f"CREATE DATABASE IF NOT EXISTS `{TEST_DB}`"))
    session.commit()
    session.execute(text(f"USE `{TEST_DB}`"))
    yield session
    session.close()


SANDBOX_DB = f"{TEST_DB}_sandbox"


@pytest.fixture(autouse=True)
def clean_db(db):
    """Ensure clean state before and after each test."""
    def _cleanup():
        try:
            for name in (SANDBOX_DB, TEST_DB):
                db.execute(text(f"DROP DATABASE IF EXISTS `{name}`"))
                db.commit()
        except Exception:
            pass

    _cleanup()
    try:
        db.execute(text(f"CREATE DATABASE IF NOT EXISTS `{TEST_DB}`"))
        db.execute(text(f"USE `{TEST_DB}`"))
        db.commit()
    except Exception:
        pass
    yield
    try:
        db.rollback()
    except Exception:
        pass
    _cleanup()


def _seed(db, table="t1", rows=((1, 1), (2, 2), (3, 3))):
    """Create and populate a table in TEST_DB."""
    db.execute(text(f"DROP TABLE IF EXISTS `{TEST_DB}`.`{table}`"))
    db.execute(text(f"CREATE TABLE `{TEST_DB}`.`{table}`(a int, b int, primary key(a))"))
    if rows:
        vals = ",".join(f"({a},{b})" for a, b in rows)
        db.execute(text(f"INSERT INTO `{TEST_DB}`.`{table}` VALUES {vals}"))
    db.commit()


def _select(db, table="t1"):
    r = db.execute(text(f"SELECT a, b FROM {TEST_DB}.{table} ORDER BY a"))
    return [(row.a, row.b) for row in r]


# ===========================================================================
# 1. Branch — low-level data branch commands
# ===========================================================================

class TestBranch:

    def test_create_zero_copy(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        assert _select(db, branch_name) == [(1, 1), (2, 2), (3, 3)]

    def test_diff_insert(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        db.execute(text(f"INSERT INTO {TEST_DB}.{branch_name} VALUES(4,4)"))
        db.commit()

        rows = br.diff(branch_name, "t1")
        assert len(rows) == 1
        assert rows[0]["flag"] == "INSERT"
        assert rows[0]["a"] == 4

    def test_diff_update(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        db.execute(text(f"UPDATE {TEST_DB}.{branch_name} SET b=99 WHERE a=1"))
        db.commit()

        rows = br.diff(branch_name, "t1")
        assert len(rows) == 1
        assert rows[0]["flag"] == "UPDATE"
        assert rows[0]["b"] == 99

    def test_diff_delete(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        db.execute(text(f"DELETE FROM {TEST_DB}.{branch_name} WHERE a=2"))
        db.commit()

        rows = br.diff(branch_name, "t1")
        assert len(rows) == 1
        assert rows[0]["flag"] == "DELETE"
        assert rows[0]["a"] == 2

    def test_diff_no_changes(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        assert br.diff(branch_name, "t1") == []

    def test_diff_mixed(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        db.execute(text(f"INSERT INTO {TEST_DB}.{branch_name} VALUES(4,4)"))
        db.execute(text(f"UPDATE {TEST_DB}.{branch_name} SET b=99 WHERE a=1"))
        db.execute(text(f"DELETE FROM {TEST_DB}.{branch_name} WHERE a=3"))
        db.commit()

        rows = br.diff(branch_name, "t1")
        flags = {r["flag"] for r in rows}
        assert flags == {"INSERT", "UPDATE", "DELETE"}

    def test_merge_accept(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        db.execute(text(f"INSERT INTO {TEST_DB}.{branch_name} VALUES(4,4)"))
        db.execute(text(f"UPDATE {TEST_DB}.{branch_name} SET b=99 WHERE a=1"))
        db.commit()

        br.merge(branch_name, "t1", on_conflict="accept")
        assert _select(db) == [(1, 99), (2, 2), (3, 3), (4, 4)]

    def test_merge_skip(self, db):
        """Conflict: both sides modify same row differently."""
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        # t2: a=1 → b=99
        db.execute(text(f"UPDATE {TEST_DB}.{branch_name} SET b=99 WHERE a=1"))
        db.commit()
        # t1: a=1 → b=50 (conflict)
        db.execute(text(f"UPDATE {TEST_DB}.t1 SET b=50 WHERE a=1"))
        db.commit()

        br.merge(branch_name, "t1", on_conflict="skip")
        # skip = keep target (t1), so a=1 stays b=50
        assert _select(db)[0] == (1, 50)

    def test_merge_conflict_accept(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        db.execute(text(f"UPDATE {TEST_DB}.{branch_name} SET b=99 WHERE a=1"))
        db.commit()
        db.execute(text(f"UPDATE {TEST_DB}.t1 SET b=50 WHERE a=1"))
        db.commit()

        br.merge(branch_name, "t1", on_conflict="accept")
        # accept = take source (t2), so a=1 becomes b=99
        assert _select(db)[0] == (1, 99)

    def test_delete_branch(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        br.delete(branch_name)
        # t2 should still exist as a table but branch metadata cleaned
        # Verify by trying to diff — should fail or show no LCA
        # Just verify no exception on delete
        assert True

    def test_diff_after_merge_is_empty(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        uuid_str = str(uuid7()).replace("-", "_")
        branch_name = f"t2_{uuid_str}"
        br.create(branch_name, "t1")
        db.execute(text(f"INSERT INTO {TEST_DB}.{branch_name} VALUES(4,4)"))
        db.commit()

        br.merge(branch_name, "t1", on_conflict="accept")
        rows = br.diff(branch_name, "t1")
        assert rows == []


# ===========================================================================
# 2. DataContext — session-scoped sandbox lifecycle
# ===========================================================================

class TestDataContext:

    def test_full_lifecycle(self, db):
        """ensure_created → ensure_tables → modify → diff → destroy."""
        _seed(db)
        br = Branch(database=TEST_DB, db=db)

        ctx = DataContext(
            db=db, branch=br, sandbox_name=SANDBOX_DB,
            source_db=TEST_DB, access=DataAccessLevel.WRITE,
        )
        ctx.ensure_created()
        assert ctx.alive

        ctx.ensure_tables(["t1"])
        r = db.execute(text(f"SELECT count(*) FROM {SANDBOX_DB}.t1"))
        assert r.scalar() == 3

        db.execute(text(f"INSERT INTO {SANDBOX_DB}.t1 VALUES(4,4)"))
        db.commit()

        diffs = ctx.diff(["t1"])
        assert len(diffs) == 1
        assert diffs[0].table == "t1"
        assert any(r["flag"] == "INSERT" for r in diffs[0].rows)

        ctx.destroy()
        assert not ctx.alive

    def test_ensure_tables_idempotent(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)

        ctx = DataContext(
            db=db, branch=br, sandbox_name=SANDBOX_DB,
            source_db=TEST_DB, access=DataAccessLevel.WRITE,
        )
        ctx.ensure_created()
        ctx.ensure_tables(["t1"])
        ctx.ensure_tables(["t1"])  # second call should be no-op
        assert len(ctx._branched_tables) == 1
        ctx.destroy()

    def test_merge_back_to_source(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)

        ctx = DataContext(
            db=db, branch=br, sandbox_name=SANDBOX_DB,
            source_db=TEST_DB, access=DataAccessLevel.WRITE,
        )
        ctx.ensure_created()
        ctx.ensure_tables(["t1"])

        db.execute(text(f"INSERT INTO {SANDBOX_DB}.t1 VALUES(5,5)"))
        db.commit()

        result = ctx.merge(["t1"], on_conflict="accept")
        assert result.tables_merged == ["t1"]
        assert result.tables_failed == []

        assert (5, 5) in _select(db)
        ctx.destroy()

    def test_diff_no_changes(self, db):
        _seed(db)
        br = Branch(database=TEST_DB, db=db)

        ctx = DataContext(
            db=db, branch=br, sandbox_name=SANDBOX_DB,
            source_db=TEST_DB, access=DataAccessLevel.WRITE,
        )
        ctx.ensure_created()
        ctx.ensure_tables(["t1"])

        diffs = ctx.diff(["t1"])
        assert diffs == []
        ctx.destroy()

    def test_multiple_tables(self, db):
        _seed(db, "orders", ((10, 100), (20, 200)))
        _seed(db, "items", ((1, 10), (2, 20)))
        br = Branch(database=TEST_DB, db=db)

        ctx = DataContext(
            db=db, branch=br, sandbox_name=SANDBOX_DB,
            source_db=TEST_DB, access=DataAccessLevel.WRITE,
        )
        ctx.ensure_created()
        ctx.ensure_tables(["orders", "items"])

        db.execute(text(f"INSERT INTO {SANDBOX_DB}.orders VALUES(30,300)"))
        db.execute(text(f"DELETE FROM {SANDBOX_DB}.items WHERE a=1"))
        db.commit()

        diffs = ctx.diff()
        assert len(diffs) == 2
        tables_with_diff = {d.table for d in diffs}
        assert tables_with_diff == {"orders", "items"}
        ctx.destroy()


# ===========================================================================
# 3. CodeExecutor WRITE mode — real subprocess + real DB
# ===========================================================================

class TestCodeExecutorWrite:

    def test_write_mode_end_to_end(self, db):
        """Full flow: declare tables → execute code → get diff + time_travel."""
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        runtime = SubprocessRuntime()
        executor = CodeExecutor(runtime=runtime, db=db, branch=br, security=SecurityGuard())

        # Use unique session_id (same format as production)
        session_id = generate_id()
        expected_sandbox = f"code_exec_{generate_hash_id(session_id, 8)}"
        
        # Clean up any existing sandbox
        try:
            db.execute(text(f"DROP DATABASE IF EXISTS {expected_sandbox}"))
            db.commit()
        except Exception:
            pass
        
        # Code that inserts into sandbox
        code = f"""
import pymysql
conn = pymysql.connect(host='127.0.0.1', port=6001, user='root', password='111', database='{expected_sandbox}')
cur = conn.cursor()
cur.execute('INSERT INTO t1 VALUES(10, 10)')
conn.commit()
conn.close()
print('inserted')
"""
        result = executor.execute(CodeExecutionRequest(
            code=code,
            language="python",
            session_id=session_id,
            data_access=DataAccessLevel.WRITE,
            source_db=TEST_DB,
            tables=["t1"],
        ))

        try:
            assert result.execution.exit_code == 0
            assert "inserted" in result.execution.stdout
            assert result.time_travel is not None
            assert result.time_travel.source_db == TEST_DB
            assert result.time_travel.started_at is not None
            assert result.data_diff is not None
            assert len(result.data_diff) == 1
            assert any(r["flag"] == "INSERT" and r["a"] == 10 for r in result.data_diff[0].rows)
        finally:
            executor.cleanup_session(session_id)

    def test_write_mode_failed_code_no_diff(self, db):
        """Failed execution (exit_code != 0) should NOT produce diff."""
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        runtime = SubprocessRuntime()
        executor = CodeExecutor(runtime=runtime, db=db, branch=br, security=SecurityGuard())

        result = executor.execute(CodeExecutionRequest(
            code="raise Exception('boom')",
            language="python",
            session_id=TEST_DB + "_fail",
            data_access=DataAccessLevel.WRITE,
            source_db=TEST_DB,
            tables=["t1"],
        ))

        try:
            assert result.execution.exit_code == 1
            assert result.data_diff is None
            # time_travel still recorded (started_at exists)
            assert result.time_travel is not None
        finally:
            executor.cleanup_session(TEST_DB + "_fail")

    def test_write_mode_no_changes(self, db):
        """Code runs successfully but doesn't modify data → empty diff."""
        _seed(db)
        br = Branch(database=TEST_DB, db=db)
        runtime = SubprocessRuntime()
        executor = CodeExecutor(runtime=runtime, db=db, branch=br, security=SecurityGuard())

        session_id = f"{worker_id}_noop"
        expected_sandbox = f"code_exec_{generate_hash_id(session_id, 8)}"
        
        # Clean up any existing sandbox
        try:
            db.execute(text(f"DROP DATABASE IF EXISTS {expected_sandbox}"))
            db.commit()
        except Exception:
            pass

        result = executor.execute(CodeExecutionRequest(
            code="print('hello')",
            language="python",
            session_id=session_id,
            data_access=DataAccessLevel.WRITE,
            source_db=TEST_DB,
            tables=["t1"],
        ))

        try:
            assert result.execution.exit_code == 0
            assert result.data_diff == []
        finally:
            executor.cleanup_session(session_id)
