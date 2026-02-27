"""Integration tests for SandboxCleaner."""

import os
import pytest
from sqlalchemy import text
from uuid_utils import uuid7

from core.sandbox import Sandbox, SandboxCleaner

SOURCE_DB = os.environ.get("MATRIXONE_DATABASE", "test_dev_agent_v3")


@pytest.fixture
def sandbox(db_session):
    return Sandbox(lambda: db_session, source_db=SOURCE_DB)


@pytest.fixture
def cleaner(db_session):
    return SandboxCleaner(lambda: db_session, source_db=SOURCE_DB)


def _name():
    return f"sandbox_{str(uuid7()).replace('-', '_')[:16]}".lower()


def test_cleanup_expired(sandbox, cleaner, db_session):
    """Sandbox with old updated_at gets cleaned up."""
    name = _name()
    sandbox.create(name)

    # Backdate updated_at to make it look expired
    db_session.execute(text(
        f"UPDATE {SOURCE_DB}.infra_sandbox_metadata "
        f"SET updated_at = '2020-01-01 00:00:00' WHERE sandbox_name = :n"
    ), {"n": name})
    db_session.commit()

    result = cleaner.run(ttl_hours=1)
    assert result["cleaned"] >= 1

    # Verify gone
    sandboxes = sandbox.list_sandboxes(prefix="", pattern=name)
    assert not any(s["sandbox_name"] == name for s in sandboxes)


def test_cleanup_skips_recent(sandbox, cleaner):
    """Recently updated sandbox is NOT cleaned."""
    name = _name()
    sandbox.create(name)

    result = cleaner.run(ttl_hours=1)
    # Should not be cleaned — just created
    sandboxes = sandbox.list_sandboxes(prefix="", pattern=name)
    assert any(s["sandbox_name"] == name for s in sandboxes)

    sandbox.delete(name)


def test_cleanup_orphan_database(sandbox, cleaner, db_session):
    """Database with sandbox_ prefix but no metadata gets force-deleted."""
    orphan = f"sandbox_orphan_{str(uuid7())[:8]}".lower()
    db_session.execute(text(f"CREATE DATABASE {orphan}"))
    db_session.commit()

    result = cleaner.run()
    assert result["cleaned"] >= 1

    # Verify gone
    r = db_session.execute(text("SHOW DATABASES"))
    dbs = [row[0] for row in r]
    assert orphan not in dbs
