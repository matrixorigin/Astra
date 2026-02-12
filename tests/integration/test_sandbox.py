"""Tests for Sandbox."""

import pytest
from uuid_utils import uuid7

from core.sandbox import Sandbox
from sdk import Database


@pytest.fixture
def db():
    return Database()


@pytest.fixture
def sandbox(db):
    return Sandbox(db=db)


def test_create_and_delete(sandbox):
    """Test sandbox creation and deletion."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    sandbox.create(name, description="Test sandbox")
    sandboxes = sandbox.list_sandboxes()
    assert any(s["sandbox_name"] == name for s in sandboxes)

    sandbox.delete(name)
    sandboxes = sandbox.list_sandboxes()
    assert not any(s["sandbox_name"] == name for s in sandboxes)


def test_list_with_filter(sandbox):
    """Test list with filtering."""
    # Create test sandboxes
    sandbox.create("sandbox_exp_test1")
    sandbox.create("sandbox_prod_test2")

    # List all
    all_sandboxes = sandbox.list_sandboxes()
    assert len(all_sandboxes) >= 2

    # Filter by pattern
    exp_sandboxes = sandbox.list_sandboxes(pattern="%exp%")
    assert any("exp" in s["sandbox_name"] for s in exp_sandboxes)

    # Cleanup
    sandbox.delete("sandbox_exp_test1")
    sandbox.delete("sandbox_prod_test2")


def test_create_from_snapshot(sandbox, db):
    """Test creating sandbox from snapshot."""
    from sdk.git_for_data import GitForData

    git = GitForData(db=db)

    # Use full UUID to avoid collisions
    snap_name = f"test_snap_{str(uuid7()).replace('-', '_')}"
    git.create_snapshot(snap_name)

    name = f"sandbox_{str(uuid7()).replace('-', '_')}"
    sandbox.create(name, from_snapshot=snap_name)

    sandboxes = sandbox.list_sandboxes()
    assert any(s["sandbox_name"] == name for s in sandboxes)

    sandbox.delete(name)
    git.drop_snapshot(snap_name)


def test_isolation(sandbox, db):
    """Test sandbox isolation."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create table in main and clean up any existing test data
    db.execute("CREATE TABLE IF NOT EXISTS test_iso (id INT)")
    db.execute("DELETE FROM test_iso")

    # Insert test data
    db.execute("INSERT INTO test_iso VALUES (1)")

    # Create sandbox
    sandbox.create(name)
    
    # Clean sandbox data (it clones from main, so may have old data)
    db.execute(f"DELETE FROM {name}.test_iso")
    db.execute(f"INSERT INTO {name}.test_iso VALUES (1)")  # Start with same data as main

    # Modify sandbox
    db.execute(f"INSERT INTO {name}.test_iso VALUES (2)")

    # Verify isolation
    current_db = db.fetchone("SELECT DATABASE() as db")["db"]
    main_count = db.fetchone(f"SELECT COUNT(*) as count FROM {current_db}.test_iso")["count"]
    sandbox_count = db.fetchone(f"SELECT COUNT(*) as count FROM {name}.test_iso")["count"]

    assert main_count == 1
    assert sandbox_count == 2

    # Cleanup
    sandbox.delete(name)
    db.execute("DROP TABLE IF EXISTS test_iso")


def test_clone_table(sandbox, db):
    """Test table cloning."""
    # Create source table
    db.execute("CREATE TABLE IF NOT EXISTS test_clone_src (id INT)")
    db.execute("INSERT INTO test_clone_src VALUES (1), (2)")

    # Clone table
    sandbox.clone_table("test_clone_dst", "test_clone_src")

    # Verify
    count = db.fetchone("SELECT COUNT(*) as count FROM test_clone_dst")["count"]
    assert count == 2

    # Cleanup
    db.execute("DROP TABLE IF EXISTS test_clone_src")
    db.execute("DROP TABLE IF EXISTS test_clone_dst")


def test_add_remove_table(sandbox, db):
    """Test add/remove table."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create empty sandbox (no tables)
    db.execute(f"CREATE DATABASE {name}")

    # Add table
    sandbox.add_table(name, "conversation_events")
    tables = sandbox.list_tables(name)
    assert "conversation_events" in tables

    # Remove table
    sandbox.remove_table(name, "conversation_events")
    tables = sandbox.list_tables(name)
    assert "conversation_events" not in tables

    # Cleanup
    sandbox.delete(name)


def test_sandbox_info(sandbox, db):
    """Test sandbox info."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    sandbox.create(name, description="Test info")
    info = sandbox.info(name)

    assert info["sandbox_name"] == name
    assert info["table_count"] > 0
    assert len(info["table_details"]) > 0

    sandbox.delete(name)


def test_sandbox_snapshot(sandbox, db):
    """Test sandbox snapshot and restore."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Create sandbox
    sandbox.create(name, description="Test snapshot")

    # Create snapshot
    sandbox.snapshot(name, "snap1")

    # List snapshots
    snapshots = sandbox.list_snapshots(name)
    assert len(snapshots) > 0
    assert any(s["name"] == "snap1" for s in snapshots)

    # Modify sandbox
    db.execute(f"DROP TABLE IF EXISTS {name}.conversation_events")

    # Restore
    sandbox.restore(name, "snap1")

    # Verify restored
    tables = sandbox.list_tables(name)
    assert "conversation_events" in tables

    # Delete sandbox (should also delete snapshots)
    sandbox.delete(name)

    # Verify snapshots are deleted
    all_snapshots = sandbox.git.list_snapshots()
    assert not any(s["snapshot_name"].startswith(f"{name}_") for s in all_snapshots)


def test_use_sandbox(sandbox, db):
    """Test switching to sandbox."""
    name = f"sandbox_{str(uuid7()).replace('-', '_')}".lower()

    # Insert test data first
    from core.events.event_logger import EventLogger
    logger = EventLogger(db)
    logger.create_user_query(user_id="test", session_id="test_session", content="test")

    # Create sandbox (clones current data)
    sandbox.create(name)

    # Switch to sandbox
    sandbox.use(name)

    # Query in sandbox (no need to prefix with sandbox name)
    count = db.fetchone("SELECT COUNT(*) as count FROM conversation_events")["count"]
    assert count > 0

    # Switch back to main
    sandbox.use("dev_agent")

    # Cleanup
    sandbox.delete(name)
